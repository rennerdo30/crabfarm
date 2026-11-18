//! Package B integration tests: farm identity, join, mutual TLS 1.3, framing.
//! Everything runs in-process on loopback with throwaway farms in tempdirs.

use crabfarm::farm::{self, Identity, Invites, JoinCode, NodeCertSpec, cert_fingerprint};
use crabfarm::proto::{BlobHash, MAX_FRAME, Message, NodeId, PROTO_VERSION};
use crabfarm::transport::{self, Accepted, FramedConn, TlsContext};
use rcgen::{ExtendedKeyUsagePurpose, KeyPair, PKCS_ED25519};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{ring, verify_tls13_signature};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, ProtocolVersion, SignatureScheme,
    version,
};

const T: Duration = Duration::from_secs(15);

async fn within<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(T, f)
        .await
        .expect("test step timed out")
}

/// What the listener saw for one inbound connection.
type Outcome = Result<&'static str, String>;

struct Listener {
    addr: String,
    outcomes: mpsc::UnboundedReceiver<Outcome>,
}

impl Listener {
    async fn next(&mut self) -> Outcome {
        within(self.outcomes.recv()).await.expect("listener closed")
    }
}

/// A node's accept loop: peers get an echo service, joins go to `handle_join`.
async fn listen(id: Identity, dir: PathBuf) -> Listener {
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap().to_string();
    let (tx, rx) = mpsc::unbounded_channel();
    let tls = Arc::new(TlsContext::new(&id).unwrap());
    let id = Arc::new(id);
    let invites = Arc::new(Mutex::new(Invites::load(&dir).unwrap()));
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = tcp.accept().await else {
                return;
            };
            let (tls, id, dir, invites, tx) = (
                tls.clone(),
                id.clone(),
                dir.clone(),
                invites.clone(),
                tx.clone(),
            );
            tokio::spawn(async move {
                match transport::accept(&tls, &id, sock).await {
                    Err(e) => {
                        let _ = tx.send(Err(format!("{e:#}")));
                    }
                    Ok(Accepted::Peer {
                        node_id,
                        hello,
                        mut conn,
                    }) => {
                        let Message::Hello {
                            node_id: hello_id, ..
                        } = hello
                        else {
                            panic!("hello")
                        };
                        assert_eq!(node_id, hello_id);
                        let _ = tx.send(Ok("peer"));
                        while let Ok(Some(m)) = conn.recv().await {
                            if conn.send(&m).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(Accepted::Join { request, mut conn }) => {
                        let reply = {
                            let mut inv = invites.lock().unwrap();
                            farm::handle_join(&id, &mut inv, &dir, &request).unwrap()
                        };
                        let _ = conn.send(&reply).await;
                        let _ = tx.send(Ok("join"));
                    }
                }
            });
        }
    });
    Listener { addr, outcomes: rx }
}

struct Node {
    dir: tempfile::TempDir,
    id: Identity,
}

fn new_farm(name: &str) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let id = farm::create(dir.path(), name).unwrap();
    Node { dir, id }
}

fn code_for(inviter: &Node, addr: &str) -> JoinCode {
    let token = Invites::load(inviter.dir.path())
        .unwrap()
        .issue(inviter.dir.path())
        .unwrap();
    JoinCode {
        token,
        cert_fp: cert_fingerprint(&inviter.id.node_cert_der),
        addr: addr.to_string(),
    }
}

/// Create farm A, start A's listener, join B. Returns (A, A's listener, B).
async fn farm_of_two() -> (Node, Listener, Node) {
    let a = new_farm("alpha");
    let mut l = listen(a.id.clone(), a.dir.path().to_path_buf()).await;
    let code = code_for(&a, &l.addr);
    let bdir = tempfile::tempdir().unwrap();
    let (b_id, peers) = within(farm::join(bdir.path(), "beta", &code))
        .await
        .unwrap();
    assert_eq!(l.next().await, Ok("join"));
    assert!(
        peers
            .iter()
            .any(|p| p.node_id == a.id.node_id && p.addrs == vec![l.addr.clone()]),
        "inviter is in the peers list"
    );
    (
        a,
        l,
        Node {
            dir: bdir,
            id: b_id,
        },
    )
}

fn chunk(n: usize) -> Message {
    let data: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
    Message::BlobChunk {
        hash: BlobHash::of(&data),
        offset: 0,
        last: true,
        data,
    }
}

// ---------------------------------------------------------------- 1. happy path

#[tokio::test]
async fn create_invite_join_connect_and_exchange_frames() {
    let (a, mut a_listener, b) = farm_of_two().await;

    // Both identities survive a reload and belong to one farm.
    let a2 = farm::load(a.dir.path()).unwrap().unwrap();
    let b2 = farm::load(b.dir.path()).unwrap().unwrap();
    assert_eq!(a2.short_farm_id(), b2.short_farm_id());
    assert!(
        a2.ca_key_der.is_some() && b2.ca_key_der.is_none(),
        "only the creator holds the CA key"
    );
    assert_ne!(a2.blob_key, b2.blob_key);

    // B -> A.
    let tls_b = TlsContext::new(&b2).unwrap();
    let (peer_id, hello, mut conn) = within(transport::connect(&tls_b, &b2, &a_listener.addr))
        .await
        .unwrap();
    assert_eq!(peer_id, a2.node_id);
    assert!(
        matches!(hello, Message::Hello { proto, node_id, .. } if proto == PROTO_VERSION && node_id == a2.node_id)
    );
    assert_eq!(a_listener.next().await, Ok("peer"));
    // The negotiated protocol is TLS 1.3 (the `tls12` feature is not compiled in at all).
    assert_eq!(
        conn.stream.get_ref().1.protocol_version(),
        Some(ProtocolVersion::TLSv1_3)
    );

    for m in [
        Message::Started,
        Message::Stderr {
            line: "hello".into(),
        },
        chunk(15 * 1024 * 1024),
    ] {
        within(conn.send(&m)).await.unwrap();
        assert_eq!(within(conn.recv()).await.unwrap().unwrap(), m);
    }
    drop(conn);

    // A -> B, the other direction.
    let mut b_listener = listen(b2.clone(), b.dir.path().to_path_buf()).await;
    let tls_a = TlsContext::new(&a2).unwrap();
    let (peer_id, _, mut conn) = within(transport::connect(&tls_a, &a2, &b_listener.addr))
        .await
        .unwrap();
    assert_eq!(peer_id, b2.node_id);
    assert_eq!(b_listener.next().await, Ok("peer"));
    within(conn.send(&Message::Cancel)).await.unwrap();
    assert_eq!(within(conn.recv()).await.unwrap().unwrap(), Message::Cancel);
}

// ---------------------------------------------------------------- 2. rejections

/// A different farm (own CA) cannot talk to this farm in either direction.
#[tokio::test]
async fn node_from_another_farm_is_refused_both_ways() {
    let a = new_farm("alpha");
    let mut a_listener = listen(a.id.clone(), a.dir.path().to_path_buf()).await;
    let c = new_farm("foreign");
    let mut c_listener = listen(c.id.clone(), c.dir.path().to_path_buf()).await;
    let tls_c = TlsContext::new(&c.id).unwrap();
    let tls_a = TlsContext::new(&a.id).unwrap();

    // Client-side check: A's client refuses C's server certificate.
    within(transport::connect(&tls_a, &a.id, &c_listener.addr))
        .await
        .expect_err("A must refuse a foreign server");
    assert!(
        c_listener.next().await.is_err(),
        "foreign listener saw a failed handshake"
    );

    // C connects to A: C's own client check already refuses A's certificate.
    assert!(
        within(transport::connect(&tls_c, &c.id, &a_listener.addr))
            .await
            .is_err()
    );
    assert!(a_listener.next().await.is_err());

    // Server-side check in isolation: C skips verifying A (test-only verifier)
    // and presents its foreign certificate; A must still refuse it.
    let cfg = accept_any_server_config(&c.id);
    let mut conn = raw_client(cfg, &a_listener.addr)
        .await
        .expect("handshake completes on the client side under TLS 1.3");
    let _ = conn.send(&hello_for(&c.id, c.id.node_id)).await;
    let a_saw = a_listener.next().await;
    assert!(
        a_saw.is_err(),
        "A accepted a client certificate from another farm: {a_saw:?}"
    );
    assert!(!matches!(
        within(conn.recv()).await,
        Ok(Some(Message::Hello { .. }))
    ));
}

/// Certificates that chain to the CA but are unfit for the role, or expired.
#[tokio::test]
async fn certificate_usage_and_validity_are_enforced() {
    let a = new_farm("alpha");
    let mut a_listener = listen(a.id.clone(), a.dir.path().to_path_buf()).await;
    let now = time::OffsetDateTime::now_utc();
    let day = time::Duration::days(1);
    let variants: [(
        &str,
        Vec<ExtendedKeyUsagePurpose>,
        time::OffsetDateTime,
        time::OffsetDateTime,
    ); 4] = [
        ("expired", both_ekus(), now - day * 30, now - day),
        ("not yet valid", both_ekus(), now + day, now + day * 30),
        (
            "no client auth",
            vec![ExtendedKeyUsagePurpose::ServerAuth],
            now - day,
            now + day,
        ),
        (
            "no server auth",
            vec![ExtendedKeyUsagePurpose::ClientAuth],
            now - day,
            now + day,
        ),
    ];
    for (label, ekus, nb, na) in variants {
        let bad = restricted_node(&a.id, ekus.clone(), nb, na);
        let tls_bad = TlsContext::new(&bad).unwrap();
        let tls_a = TlsContext::new(&a.id).unwrap();
        let usable_as_client =
            label != "no client auth" && label != "expired" && label != "not yet valid";
        let usable_as_server =
            label != "no server auth" && label != "expired" && label != "not yet valid";

        // Bad node as the CLIENT of a good listener.
        let r = within(transport::connect(&tls_bad, &bad, &a_listener.addr)).await;
        assert_eq!(r.is_ok(), usable_as_client, "{label}: as client");
        let seen = a_listener.next().await;
        assert_eq!(
            seen.is_ok(),
            usable_as_client,
            "{label}: listener view {seen:?}"
        );

        // Bad node as the SERVER seen by a good client.
        let mut bad_listener = listen(bad.clone(), tempfile::tempdir().unwrap().keep()).await;
        let r = within(transport::connect(&tls_a, &a.id, &bad_listener.addr)).await;
        assert_eq!(r.is_ok(), usable_as_server, "{label}: as server");
        let _ = bad_listener.next().await;
    }
}

fn both_ekus() -> Vec<ExtendedKeyUsagePurpose> {
    vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ]
}

/// A node of A's farm whose certificate is deliberately restricted.
fn restricted_node(
    ca: &Identity,
    ekus: Vec<ExtendedKeyUsagePurpose>,
    nb: time::OffsetDateTime,
    na: time::OffsetDateTime,
) -> Identity {
    let kp = KeyPair::generate_for(&PKCS_ED25519).unwrap();
    let node_id = NodeId(rand_id());
    let spec = NodeCertSpec {
        node_name: "restricted",
        node_id,
        ekus,
        not_before: nb,
        not_after: na,
    };
    let cert = farm::sign_node_cert_with(ca, &spec, kp.public_key_raw()).unwrap();
    Identity {
        node_id,
        node_name: "restricted".into(),
        node_key_der: kp.serialize_der(),
        node_cert_der: cert,
        ca_key_der: None,
        ..ca.clone()
    }
}

fn rand_id() -> [u8; 16] {
    let mut b = [0u8; 16];
    rand::fill(&mut b[..]);
    b
}

#[derive(Debug)]
struct AcceptAny;

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::General("tls12".into()))
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            m,
            c,
            d,
            &ring::default_provider().signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Test-only client that trusts any server but presents `id`'s certificate.
fn accept_any_server_config(id: &Identity) -> Arc<ClientConfig> {
    let chain = vec![CertificateDer::from(id.node_cert_der.clone())];
    let key = tokio_rustls::rustls::pki_types::PrivateKeyDer::Pkcs8(id.node_key_der.clone().into());
    Arc::new(
        ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_protocol_versions(&[&version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny))
            .with_client_auth_cert(chain, key)
            .unwrap(),
    )
}

async fn raw_client(
    cfg: Arc<ClientConfig>,
    addr: &str,
) -> anyhow::Result<FramedConn<transport::ClientStream>> {
    let tcp = TcpStream::connect(addr).await?;
    let s = tokio_rustls::TlsConnector::from(cfg)
        .connect(ServerName::try_from("crabfarm.local")?, tcp)
        .await?;
    Ok(FramedConn::new(s))
}

fn hello_for(id: &Identity, claimed: NodeId) -> Message {
    Message::Hello {
        proto: PROTO_VERSION,
        node_id: claimed,
        node_name: id.node_name.clone(),
        host_triple: "x".into(),
        crabfarm_version: "t".into(),
    }
}

#[tokio::test]
async fn client_without_certificate_is_refused_unless_it_joins() {
    let (a, mut l, _b) = farm_of_two().await;
    let pinned = transport::join_client_config(cert_fingerprint(&a.id.node_cert_der)).unwrap();

    // A Hello without any client certificate.
    let mut conn = raw_client(pinned.clone(), &l.addr).await.unwrap();
    conn.send(&hello_for(&a.id, NodeId([3; 16]))).await.unwrap();
    let seen = l.next().await;
    assert!(
        seen.as_ref().is_err_and(|e| e.contains("no certificate")),
        "cert-less Hello was accepted: {seen:?}"
    );
    assert!(
        !matches!(within(conn.recv()).await, Ok(Some(Message::Hello { .. }))),
        "no Hello back"
    );

    // Any other first frame is refused too.
    let mut conn = raw_client(pinned.clone(), &l.addr).await.unwrap();
    conn.send(&Message::QueryStatus).await.unwrap();
    assert!(l.next().await.is_err());

    // A JoinRequest is the one frame allowed without a certificate (bad token here).
    let mut conn = raw_client(pinned, &l.addr).await.unwrap();
    let req = Message::JoinRequest {
        token: [1; 16],
        node_name: "x".into(),
        public_key: vec![0; 32],
    };
    conn.send(&req).await.unwrap();
    assert_eq!(l.next().await, Ok("join"));
    assert!(matches!(
        within(conn.recv()).await.unwrap(),
        Some(Message::Error { .. })
    ));
}

#[tokio::test]
async fn hello_must_match_certificate_and_protocol() {
    let (a, mut l, b) = farm_of_two().await;
    let cfg = transport::client_config(&b.id).unwrap();

    // Client lies about its node id.
    let mut conn = raw_client(cfg.clone(), &l.addr).await.unwrap();
    conn.send(&hello_for(&b.id, a.id.node_id)).await.unwrap();
    let seen = l.next().await;
    assert!(
        seen.as_ref().is_err_and(|e| e.contains("node id")),
        "{seen:?}"
    );

    // Client speaks another protocol version: refused with an Error frame.
    let mut conn2 = raw_client(cfg.clone(), &l.addr).await.unwrap();
    let Message::Hello {
        node_id,
        node_name,
        host_triple,
        crabfarm_version,
        ..
    } = hello_for(&b.id, b.id.node_id)
    else {
        unreachable!()
    };
    conn2
        .send(&Message::Hello {
            proto: PROTO_VERSION + 1,
            node_id,
            node_name,
            host_triple,
            crabfarm_version,
        })
        .await
        .unwrap();
    assert!(l.next().await.is_err());
    assert!(matches!(
        within(conn2.recv()).await,
        Ok(Some(Message::Error { .. }))
    ));
    drop(conn);

    // Server lies about its node id: `connect` must refuse. The impostor is a
    // legitimate farm member (valid certificate) answering with A's id.
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap().to_string();
    let b_tls = TlsContext::new(&b.id).unwrap();
    let victim = a.id.node_id;
    tokio::spawn(async move {
        let (sock, _) = tcp.accept().await.unwrap();
        let s = tokio_rustls::TlsAcceptor::from(b_tls.server.clone())
            .accept(sock)
            .await
            .unwrap();
        let mut c = FramedConn::new(s);
        let _ = c.recv().await;
        let fake = Message::Hello {
            proto: PROTO_VERSION,
            node_id: victim,
            node_name: "b".into(),
            host_triple: "x".into(),
            crabfarm_version: "t".into(),
        };
        let _ = c.send(&fake).await;
        let _ = c.stream.shutdown().await;
    });
    let tls_a = TlsContext::new(&a.id).unwrap();
    let e = within(transport::connect(&tls_a, &a.id, &addr))
        .await
        .expect_err("must reject mismatched Hello");
    assert!(format!("{e:#}").contains("node id"), "{e:#}");
}

#[tokio::test]
async fn join_tokens_wrong_reused_expired_and_pin() {
    let a = new_farm("alpha");
    let mut l = listen(a.id.clone(), a.dir.path().to_path_buf()).await;
    let good = code_for(&a, &l.addr);

    // Wrong token.
    let d = tempfile::tempdir().unwrap();
    let wrong = JoinCode {
        token: [0xee; 16],
        ..good.clone()
    };
    let e = within(farm::join(d.path(), "w", &wrong))
        .await
        .expect_err("wrong token");
    assert!(format!("{e:#}").contains("refused"), "{e:#}");
    assert_eq!(l.next().await, Ok("join"));
    assert!(
        farm::load(d.path()).unwrap().is_none(),
        "nothing written after a refused join"
    );

    // Wrong pin: fails in the handshake, before the token is ever sent.
    let bad_pin = JoinCode {
        cert_fp: [0x11; 16],
        ..good.clone()
    };
    assert!(within(farm::join(d.path(), "p", &bad_pin)).await.is_err());
    assert!(
        l.next().await.is_err(),
        "inviter saw only a failed handshake"
    );
    assert!(farm::load(d.path()).unwrap().is_none());

    // The good code still works (neither failure consumed it) ...
    let (_id, _) = within(farm::join(d.path(), "ok", &good)).await.unwrap();
    assert_eq!(l.next().await, Ok("join"));

    // ... exactly once.
    let d2 = tempfile::tempdir().unwrap();
    assert!(
        within(farm::join(d2.path(), "again", &good)).await.is_err(),
        "reused token"
    );
    assert_eq!(l.next().await, Ok("join"));
    assert!(farm::load(d2.path()).unwrap().is_none());

    // Expired: an invite whose expiry is in the past (hand-written entry).
    let token = [0x42u8; 16];
    let hash: String = Sha256::digest(token)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    write_invites(
        a.dir.path(),
        &format!("[[invites]]\nhash = \"{hash}\"\nexpires = 1\n"),
    );
    let d3 = tempfile::tempdir().unwrap();
    let expired = JoinCode {
        token,
        ..good.clone()
    };
    assert!(
        within(farm::join(d3.path(), "late", &expired))
            .await
            .is_err(),
        "expired token"
    );
    assert_eq!(l.next().await, Ok("join"));
    // The same entry with a future expiry works, proving the entry itself was fine.
    let future = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 300;
    write_invites(
        a.dir.path(),
        &format!("[[invites]]\nhash = \"{hash}\"\nexpires = {future}\n"),
    );
    within(farm::join(d3.path(), "ontime", &expired))
        .await
        .unwrap();
}

fn write_invites(dir: &Path, text: &str) {
    std::fs::write(dir.join("farm").join("invites.toml"), text).unwrap();
}

/// A node that joined (no CA key) cannot hand out joins even with a valid invite.
#[tokio::test]
async fn only_the_ca_holder_signs_joins() {
    let (_a, _l, b) = farm_of_two().await;
    let mut lb = listen(b.id.clone(), b.dir.path().to_path_buf()).await;
    std::fs::create_dir_all(b.dir.path().join("farm")).unwrap();
    let code = code_for(&b, &lb.addr);
    let d = tempfile::tempdir().unwrap();
    let e = within(farm::join(d.path(), "c", &code))
        .await
        .expect_err("non-CA node must not sign");
    assert!(format!("{e:#}").contains("refused"));
    assert_eq!(lb.next().await, Ok("join"));
}

// ---------------------------------------------------------------- framing

#[tokio::test]
async fn oversized_frame_is_refused_without_reading_it() {
    let (mut tx, rx) = tokio::io::duplex(1024);
    let mut conn = FramedConn::new(rx);
    // Header announces MAX_FRAME + 1 bytes and nothing follows: a reader that
    // trusted the length would wait for the body (the timeout would fire).
    tx.write_all(&((MAX_FRAME as u32) + 1).to_le_bytes())
        .await
        .unwrap();
    let r = tokio::time::timeout(Duration::from_secs(2), conn.recv())
        .await
        .expect("recv must fail fast, not wait for the body");
    assert!(r.unwrap_err().to_string().contains("exceeds"));

    // A frame exactly at the limit is not refused by the size check (it fails
    // later, as the peer closes mid-body).
    let (mut tx, rx) = tokio::io::duplex(1024);
    let mut conn = FramedConn::new(rx);
    tx.write_all(&(MAX_FRAME as u32).to_le_bytes())
        .await
        .unwrap();
    tx.write_all(&[0u8; 10]).await.unwrap();
    drop(tx);
    let e = conn.recv().await.unwrap_err();
    assert!(e.to_string().contains("closed inside a frame"), "{e}");

    // Sending one is refused locally.
    let (a, _b) = tokio::io::duplex(64);
    let mut conn = FramedConn::new(a);
    let big = Message::Stderr {
        line: "x".repeat(MAX_FRAME + 1),
    };
    assert!(conn.send(&big).await.is_err());
}

#[tokio::test]
async fn framing_round_trip_eof_and_garbage() {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (mut a, mut b) = (FramedConn::new(a), FramedConn::new(b));
    let m = Message::Exit { code: -3 };
    a.send(&m).await.unwrap();
    assert_eq!(b.recv().await.unwrap(), Some(m));
    drop(a);
    assert_eq!(b.recv().await.unwrap(), None, "clean EOF");

    let (mut w, r) = tokio::io::duplex(64);
    let mut r = FramedConn::new(r);
    w.write_all(&[2, 0, 0, 0, 0xff, 0xff]).await.unwrap();
    assert!(
        r.recv().await.is_err(),
        "undecodable frame is an error, not a panic"
    );
    let (mut w, r) = tokio::io::duplex(64);
    let mut r = FramedConn::new(r);
    w.write_all(&[1, 0]).await.unwrap();
    drop(w);
    assert!(r.recv().await.is_err(), "EOF inside the header");
}

/// Before authentication only small frames are read.
#[tokio::test]
async fn unauthenticated_peer_cannot_send_large_first_frame() {
    let a = new_farm("alpha");
    let mut l = listen(a.id.clone(), a.dir.path().to_path_buf()).await;
    let pinned = transport::join_client_config(cert_fingerprint(&a.id.node_cert_der)).unwrap();
    let mut conn = raw_client(pinned, &l.addr).await.unwrap();
    // The send itself may succeed (it is buffered); the listener refuses on the header.
    let _ = conn
        .send(&Message::Stderr {
            line: "x".repeat(200 * 1024),
        })
        .await;
    let seen = l.next().await;
    assert!(
        seen.as_ref().is_err_and(|e| e.contains("exceeds")),
        "{seen:?}"
    );
}
