//! Package B. Framing and mutual TLS 1.3 (DESIGN sections 3 and 11).

use crate::farm::{Identity, cert_fingerprint, node_id_from_cert};
use crate::proto::{MAX_FRAME, Message, NodeId, PROTO_VERSION};
use anyhow::{Context, bail};
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::client::WebPkiServerVerifier;
use tokio_rustls::rustls::crypto::{CryptoProvider, ring as ring_provider, verify_tls13_signature};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, ServerConfig,
    SignatureScheme, version,
};

/// Connect + handshake budget.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Frames accepted before the peer is authenticated (Hello, JoinRequest).
const PRE_AUTH_MAX_FRAME: usize = 64 * 1024;
/// The name offered for SNI; verification ignores hostnames (DESIGN section 3).
const SERVER_NAME: &str = "crabfarm.local";

/// Length-prefixed postcard framing over any async stream.
pub struct FramedConn<S> {
    pub stream: S,
}

impl<S> std::fmt::Debug for FramedConn<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FramedConn")
    }
}

impl<S> FramedConn<S> {
    pub fn new(stream: S) -> Self {
        FramedConn { stream }
    }
}

// Reading and writing are separate impls so a connection can be split into
// halves (`tokio::io::split`) and each half framed on its own.
impl<S: AsyncWrite + Unpin + Send> FramedConn<S> {
    /// Encode and write one frame (u32 LE length + postcard), flush.
    pub async fn send(&mut self, msg: &Message) -> anyhow::Result<()> {
        let body = postcard::to_stdvec(msg).context("encode frame")?;
        if body.len() > MAX_FRAME {
            bail!("frame of {} bytes exceeds the {} byte limit", body.len(), MAX_FRAME);
        }
        // One write so the length prefix and body leave in the same TLS record burst.
        let mut buf = Vec::with_capacity(4 + body.len());
        buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        buf.extend_from_slice(&body);
        self.stream.write_all(&buf).await.context("write frame")?;
        self.stream.flush().await.context("flush frame")?;
        Ok(())
    }

}

impl<S: AsyncRead + Unpin + Send> FramedConn<S> {
    /// Read one frame; `Ok(None)` on clean EOF; error above MAX_FRAME.
    /// Not cancel-safe: drop the connection if a `recv` future is dropped
    /// mid-frame (use it inside `timeout` only when the connection is then closed).
    pub async fn recv(&mut self) -> anyhow::Result<Option<Message>> {
        self.recv_max(MAX_FRAME).await
    }

    /// Like `recv` with a tighter size cap (used before the peer is trusted).
    pub async fn recv_max(&mut self, max: usize) -> anyhow::Result<Option<Message>> {
        let mut hdr = [0u8; 4];
        let mut got = 0;
        while got < 4 {
            let n = self.stream.read(&mut hdr[got..]).await.context("read frame header")?;
            if n == 0 {
                if got == 0 {
                    return Ok(None);
                }
                bail!("connection closed inside a frame header");
            }
            got += n;
        }
        let len = u32::from_le_bytes(hdr) as usize;
        // Checked before any allocation: a hostile length prefix costs nothing.
        if len > max {
            bail!("frame of {len} bytes exceeds the {max} byte limit");
        }
        // Grow with the bytes that actually arrive instead of trusting `len`.
        let mut body = Vec::new();
        let read = (&mut self.stream).take(len as u64).read_to_end(&mut body).await.context("read frame body")?;
        if read != len {
            bail!("connection closed inside a frame ({read} of {len} bytes)");
        }
        Ok(Some(postcard::from_bytes(&body).context("decode frame")?))
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring_provider::default_provider())
}

fn farm_roots(id: &Identity) -> anyhow::Result<Arc<RootCertStore>> {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(id.ca_cert_der.clone())).context("farm CA certificate")?;
    Ok(Arc::new(roots))
}

fn own_cert_and_key(id: &Identity) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    (
        vec![CertificateDer::from(id.node_cert_der.clone())],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(id.node_key_der.clone())),
    )
}

/// Verifies a server's end-entity certificate against the farm CA only
/// (chain, validity time, server-auth usage; all done by rustls-webpki) and
/// ignores the hostname: nodes are addressed by IP or any DNS name, and
/// identity is the node id in the certificate. webpki checks the chain first
/// and the name last, so tolerating exactly the name-mismatch error keeps
/// every other check.
#[derive(Debug)]
struct FarmServerVerifier {
    inner: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for FarmServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        match self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now) {
            Err(TlsError::InvalidCertificate(CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. })) => {
                Ok(ServerCertVerified::assertion())
            }
            other => other,
        }
    }

    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::PeerIncompatible(tokio_rustls::rustls::PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Join path: the joiner has no CA yet and trusts exactly the certificate
/// whose SHA-256 prefix is in the join code.
#[derive(Debug)]
struct PinnedVerifier {
    fp: [u8; 16],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let got = cert_fingerprint(end_entity);
        if bool::from(got.ct_eq(&self.fp)) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::InvalidCertificate(CertificateError::ApplicationVerificationFailure))
        }
    }

    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::PeerIncompatible(tokio_rustls::rustls::PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        // The pin fixes the certificate; the handshake signature must still
        // prove the server holds its key.
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// rustls configs for the farm: TLS 1.3 only, ring provider, both sides
/// present certs that must be signed by the farm CA (pinned by DER).
pub struct TlsContext {
    pub server: Arc<ServerConfig>,
    pub client: Arc<ClientConfig>,
    /// Advertised in `Hello`. Defaults to an OS/arch guess; the node sets the
    /// real rustc host triple.
    pub host_triple: String,
}

fn default_host_triple() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

impl TlsContext {
    pub fn new(id: &Identity) -> anyhow::Result<TlsContext> {
        Ok(TlsContext { server: server_config_optional_client_auth(id)?, client: client_config(id)?, host_triple: default_host_triple() })
    }

    fn hello(&self, id: &Identity) -> Message {
        Message::Hello {
            proto: PROTO_VERSION,
            node_id: id.node_id,
            node_name: id.node_name.clone(),
            host_triple: self.host_triple.clone(),
            crabfarm_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// Client config for peer connections: presents our node cert, accepts only
/// servers certified by the farm CA.
pub fn client_config(id: &Identity) -> anyhow::Result<Arc<ClientConfig>> {
    let p = provider();
    let inner = WebPkiServerVerifier::builder_with_provider(farm_roots(id)?, p.clone()).build().context("server verifier")?;
    let (chain, key) = own_cert_and_key(id);
    let cfg = ClientConfig::builder_with_provider(p)
        .with_protocol_versions(&[&version::TLS13])
        .context("TLS 1.3")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(FarmServerVerifier { inner }))
        .with_client_auth_cert(chain, key)
        .context("node key does not match node certificate")?;
    Ok(Arc::new(cfg))
}

pub type ClientStream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;
pub type ServerStream = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;

/// First certificate of the peer's chain (leaf), if it sent any.
fn peer_leaf(conn: &tokio_rustls::rustls::CommonState) -> Option<Vec<u8>> {
    conn.peer_certificates().and_then(|c| c.first()).map(|c| c.to_vec())
}

/// Connect to a peer (`host:port`), TLS handshake, exchange `Hello`.
/// Timeout 5 s for connect + handshake. Returns the peer's node id (from its
/// certificate, checked against its Hello) and the framed stream.
pub async fn connect(tls: &TlsContext, id: &Identity, addr: &str) -> anyhow::Result<(NodeId, Message, FramedConn<ClientStream>)> {
    let handshake = async {
        let tcp = tokio::net::TcpStream::connect(addr).await.with_context(|| format!("connect to {addr}"))?;
        tcp.set_nodelay(true).ok();
        let name = ServerName::try_from(SERVER_NAME).context("server name")?;
        tokio_rustls::TlsConnector::from(tls.client.clone()).connect(name, tcp).await.context("TLS handshake")
    };
    let stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await.with_context(|| format!("connect to {addr} timed out"))??;
    let cert_id = node_id_from_cert(&peer_leaf(stream.get_ref().1).context("peer sent no certificate")?)?;
    let mut conn = FramedConn::new(stream);
    conn.send(&tls.hello(id)).await?;
    // A TLS 1.3 client finishes its handshake before the server has judged the
    // client certificate, so a refusal shows up here as a failed read.
    let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.recv_max(PRE_AUTH_MAX_FRAME))
        .await
        .context("peer did not answer Hello in time")?
        .context("peer closed or refused the connection")?;
    let hello = match first {
        Some(h @ Message::Hello { .. }) => h,
        Some(Message::Error { message }) => bail!("peer refused: {}", crate::proto::log_safe(&message)),
        Some(_) => bail!("peer did not start with Hello"),
        None => bail!("peer closed the connection before Hello"),
    };
    check_hello(&hello, cert_id)?;
    Ok((cert_id, hello, conn))
}

/// A Hello must carry our protocol version and the node id of the
/// certificate it arrived under; otherwise a farm member could pose as another.
fn check_hello(hello: &Message, cert_id: NodeId) -> anyhow::Result<()> {
    let Message::Hello { proto, node_id, .. } = hello else { bail!("not a Hello") };
    if *proto != PROTO_VERSION {
        bail!("peer speaks protocol {proto}, this node speaks {PROTO_VERSION}");
    }
    if *node_id != cert_id {
        bail!("Hello node id does not match the peer certificate");
    }
    Ok(())
}

/// What an accepted inbound connection turned out to be.
pub enum Accepted {
    /// An authenticated farm member (certificate verified against the farm CA,
    /// node id equal to the one in its Hello).
    Peer { node_id: NodeId, hello: Message, conn: FramedConn<ServerStream> },
    /// A first frame `JoinRequest` from a machine that may have no certificate.
    /// The caller passes `request` to `farm::handle_join` and answers on `conn`.
    Join { request: Message, conn: FramedConn<ServerStream> },
}

/// Server side of `connect` for an accepted TCP stream. Every connection
/// whose first frame is not a `JoinRequest` must present a farm-signed
/// client certificate.
pub async fn accept(tls: &TlsContext, id: &Identity, tcp: tokio::net::TcpStream) -> anyhow::Result<Accepted> {
    tcp.set_nodelay(true).ok();
    let stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, tokio_rustls::TlsAcceptor::from(tls.server.clone()).accept(tcp))
        .await
        .context("TLS handshake timed out")?
        .context("TLS handshake")?;
    let client_cert = peer_leaf(stream.get_ref().1);
    let mut conn = FramedConn::new(stream);
    let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.recv_max(PRE_AUTH_MAX_FRAME))
        .await
        .context("no first frame in time")??
        .context("peer closed before its first frame")?;
    match first {
        request @ Message::JoinRequest { .. } => Ok(Accepted::Join { request, conn }),
        hello @ Message::Hello { .. } => {
            let cert = client_cert.context("client presented no certificate")?;
            let cert_id = node_id_from_cert(&cert)?;
            if let Err(e) = check_hello(&hello, cert_id) {
                // Best effort so an honest but outdated peer learns why.
                let _ = conn.send(&Message::Error { message: e.to_string() }).await;
                return Err(e);
            }
            conn.send(&tls.hello(id)).await?;
            Ok(Accepted::Peer { node_id: cert_id, hello, conn })
        }
        _ => bail!("first frame was neither Hello nor JoinRequest"),
    }
}

/// Join handshake client config: no client certificate, and the server
/// certificate is pinned by SHA-256 fingerprint prefix instead of the CA.
pub fn join_client_config(cert_fp: [u8; 16]) -> anyhow::Result<Arc<ClientConfig>> {
    let p = provider();
    let cfg = ClientConfig::builder_with_provider(p.clone())
        .with_protocol_versions(&[&version::TLS13])
        .context("TLS 1.3")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier { fp: cert_fp, provider: p }))
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// Server config used for every inbound connection: client certs are
/// *optional* at the TLS layer (a joining node has none); `accept` then
/// requires a valid farm cert for everything except a `JoinRequest` first
/// frame. A certificate that IS presented must chain to the farm CA, or the
/// handshake fails.
pub fn server_config_optional_client_auth(id: &Identity) -> anyhow::Result<Arc<ServerConfig>> {
    let p = provider();
    let verifier = WebPkiClientVerifier::builder_with_provider(farm_roots(id)?, p.clone())
        .allow_unauthenticated()
        .build()
        .context("client verifier")?;
    let (chain, key) = own_cert_and_key(id);
    let cfg = ServerConfig::builder_with_provider(p)
        .with_protocol_versions(&[&version::TLS13])
        .context("TLS 1.3")?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)
        .context("node key does not match node certificate")?;
    Ok(Arc::new(cfg))
}
