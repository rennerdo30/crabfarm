//! Package B. Farm identity: CA, node certificates, invites and joining
//! (DESIGN section 3).

use crate::proto::{Message, NodeId, PROTO_VERSION, PeerInfo};
use anyhow::{Context, bail, ensure};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, PKCS_ED25519, SanType, SubjectPublicKeyInfo,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName};

/// How long an invite stays valid.
pub const INVITE_TTL_SECS: u64 = 10 * 60;
/// Upper bound for pending invites so a runaway script cannot grow the file.
const MAX_PENDING_INVITES: usize = 32;
const NODE_ID_URI_PREFIX: &str = "crabfarm:node:";
const MAX_NODE_NAME: usize = 64;
/// DER prefix of an Ed25519 SubjectPublicKeyInfo; the 32 raw key bytes follow.
const ED25519_SPKI_PREFIX: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];

/// Loaded identity of this node.
#[derive(Clone)]
pub struct Identity {
    pub node_id: NodeId,
    pub node_name: String,
    /// Ed25519 private key, PKCS#8 DER.
    pub node_key_der: Vec<u8>,
    pub node_cert_der: Vec<u8>,
    pub ca_cert_der: Vec<u8>,
    /// Present only on the farm creator.
    pub ca_key_der: Option<Vec<u8>>,
    /// Submitter blob key (DESIGN section 3, at rest).
    pub blob_key: [u8; 32],
}

// Hand-written so that keys can never end up in a log through `{:?}`.
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("node_id", &self.node_id)
            .field("node_name", &self.node_name)
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// First 16 bytes of SHA-256(CA cert DER), lowercase hex.
    pub fn short_farm_id(&self) -> String {
        hex(&cert_fingerprint(&self.ca_cert_der))
    }
}

/// First 16 bytes of SHA-256 of a certificate's DER (join-code pin, farm id).
pub fn cert_fingerprint(der: &[u8]) -> [u8; 16] {
    let digest = Sha256::digest(der);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::fill(&mut b[..]);
    b
}

// ---------------------------------------------------------------- files

/// Write a secret file owner-only. Unix: mode 0600 from creation. Windows:
/// the file lives in the per-user data dir and inherits the profile ACL;
/// explicit ACL hardening is P2. The content goes to a temp file first so a
/// crash never leaves a truncated key under the real name.
fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut tmp_name = path.file_name().context("file name")?.to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    f.write_all(data).with_context(|| format!("write {}", tmp.display()))?;
    f.sync_all().ok();
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

fn read_opt(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn read_req(path: &Path) -> anyhow::Result<Vec<u8>> {
    read_opt(path)?.with_context(|| format!("missing identity file {}", path.display()))
}

struct IdentityPaths {
    ca_crt: PathBuf,
    ca_key: PathBuf,
    node_key: PathBuf,
    node_crt: PathBuf,
    node_id: PathBuf,
    blob_key: PathBuf,
}

fn paths(data_dir: &Path) -> IdentityPaths {
    IdentityPaths {
        ca_crt: data_dir.join("farm").join("ca.crt"),
        ca_key: data_dir.join("farm").join("ca.key"),
        node_key: data_dir.join("node.key"),
        node_crt: data_dir.join("node.crt"),
        node_id: data_dir.join("node.id"),
        blob_key: data_dir.join("blob.key"),
    }
}

// ---------------------------------------------------------------- certs

fn validate_node_name(name: &str) -> anyhow::Result<()> {
    ensure!(!name.is_empty(), "node name is empty");
    ensure!(name.chars().count() <= MAX_NODE_NAME, "node name longer than {MAX_NODE_NAME} characters");
    ensure!(!name.chars().any(char::is_control), "node name contains control characters");
    Ok(())
}

fn validity_from_now(back_secs: i64, ahead_secs: i64) -> anyhow::Result<(time::OffsetDateTime, time::OffsetDateTime)> {
    let now = unix_now() as i64;
    Ok((
        time::OffsetDateTime::from_unix_timestamp(now - back_secs)?,
        time::OffsetDateTime::from_unix_timestamp(now + ahead_secs)?,
    ))
}

const TEN_YEARS_SECS: i64 = 10 * 365 * 24 * 3600;
/// Back-dated start tolerates clock skew between farm machines.
const SKEW_SECS: i64 = 24 * 3600;

fn ca_params() -> anyhow::Result<CertificateParams> {
    let mut p = CertificateParams::new(Vec::<String>::new())?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "crabfarm farm CA");
    p.distinguished_name = dn;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    (p.not_before, p.not_after) = validity_from_now(SKEW_SECS, TEN_YEARS_SECS)?;
    Ok(p)
}

/// What a node certificate may be used for. Public only so integration tests can build deliberately
/// restricted certificates; signing needs the CA key, so this grants nothing.
pub struct NodeCertSpec<'a> {
    pub node_name: &'a str,
    pub node_id: NodeId,
    pub ekus: Vec<ExtendedKeyUsagePurpose>,
    pub not_before: time::OffsetDateTime,
    pub not_after: time::OffsetDateTime,
}

fn node_cert_params(spec: &NodeCertSpec<'_>) -> anyhow::Result<CertificateParams> {
    validate_node_name(spec.node_name)?;
    let mut p = CertificateParams::new(Vec::<String>::new())?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, spec.node_name);
    p.distinguished_name = dn;
    let uri = format!("{NODE_ID_URI_PREFIX}{}", spec.node_id.to_hex());
    p.subject_alt_names = vec![SanType::URI(uri.try_into().context("node id SAN")?)];
    p.is_ca = IsCa::ExplicitNoCa;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = spec.ekus.clone();
    p.not_before = spec.not_before;
    p.not_after = spec.not_after;
    Ok(p)
}

fn ed25519_spki(public_key: &[u8]) -> anyhow::Result<SubjectPublicKeyInfo> {
    ensure!(public_key.len() == 32, "public key must be a raw 32-byte Ed25519 key");
    let mut der = ED25519_SPKI_PREFIX.to_vec();
    der.extend_from_slice(public_key);
    Ok(SubjectPublicKeyInfo::from_der(&der)?)
}

fn load_keypair(pkcs8: &[u8]) -> anyhow::Result<KeyPair> {
    Ok(KeyPair::from_pkcs8_der_and_sign_algo(&PrivatePkcs8KeyDer::from(pkcs8), &PKCS_ED25519)?)
}

pub fn sign_node_cert_with(id: &Identity, spec: &NodeCertSpec<'_>, public_key: &[u8]) -> anyhow::Result<Vec<u8>> {
    let ca_key = id.ca_key_der.as_ref().context("this node does not hold the farm CA key")?;
    let issuer = Issuer::from_ca_cert_der(&CertificateDer::from(id.ca_cert_der.as_slice()), load_keypair(ca_key)?)?;
    let params = node_cert_params(spec)?;
    let cert = params.signed_by(&ed25519_spki(public_key)?, &issuer)?;
    Ok(cert.der().to_vec())
}

/// CA side: sign a node certificate for `public_key` (raw Ed25519, 32 bytes).
/// CN = node_name, SAN URI `crabfarm:node:<node id hex>`. 10-year validity.
/// Both server-auth and client-auth EKUs: every node is both.
pub fn sign_node_cert(id: &Identity, node_name: &str, node_id: NodeId, public_key: &[u8]) -> anyhow::Result<Vec<u8>> {
    let (not_before, not_after) = validity_from_now(SKEW_SECS, TEN_YEARS_SECS)?;
    let spec = NodeCertSpec {
        node_name,
        node_id,
        ekus: vec![ExtendedKeyUsagePurpose::ServerAuth, ExtendedKeyUsagePurpose::ClientAuth],
        not_before,
        not_after,
    };
    sign_node_cert_with(id, &spec, public_key)
}

/// Extract the node id from a peer certificate (SAN URI).
pub fn node_id_from_cert(cert_der: &[u8]) -> anyhow::Result<NodeId> {
    use x509_parser::extensions::{GeneralName, ParsedExtension};
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der).map_err(|e| anyhow::anyhow!("parse certificate: {e}"))?;
    let mut found: Option<NodeId> = None;
    for ext in cert.extensions() {
        if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
            for name in &san.general_names {
                if let GeneralName::URI(uri) = name
                    && let Some(h) = uri.strip_prefix(NODE_ID_URI_PREFIX)
                {
                    ensure!(found.is_none(), "certificate carries more than one node id");
                    let bytes = data_encoding::HEXLOWER.decode(h.as_bytes()).context("node id is not lowercase hex")?;
                    let arr: [u8; 16] = bytes.try_into().map_err(|_| anyhow::anyhow!("node id is not 16 bytes"))?;
                    found = Some(NodeId(arr));
                }
            }
        }
    }
    found.context("certificate has no crabfarm node id")
}

/// CN of a certificate (the node name), for display only.
pub fn node_name_from_cert(cert_der: &[u8]) -> anyhow::Result<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der).map_err(|e| anyhow::anyhow!("parse certificate: {e}"))?;
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .context("certificate has no common name")?
        .as_str()
        .map_err(|e| anyhow::anyhow!("common name: {e}"))?;
    Ok(cn.to_string())
}

// ---------------------------------------------------------------- create / load

impl Identity {
    fn write_files(&self, data_dir: &Path) -> anyhow::Result<()> {
        let p = paths(data_dir);
        write_private(&p.ca_crt, &self.ca_cert_der)?;
        if let Some(k) = &self.ca_key_der {
            write_private(&p.ca_key, k)?;
        }
        write_private(&p.node_key, &self.node_key_der)?;
        write_private(&p.blob_key, &self.blob_key)?;
        write_private(&p.node_id, self.node_id.to_hex().as_bytes())?;
        // Written last: its presence marks a complete identity (`load`).
        write_private(&p.node_crt, &self.node_cert_der)?;
        Ok(())
    }
}

/// Create a new farm in `data_dir` (fails if one exists): CA key + cert,
/// node key + cert signed by the CA, node id, blob key. Files owner-only.
pub fn create(data_dir: &Path, node_name: &str) -> anyhow::Result<Identity> {
    validate_node_name(node_name)?;
    let p = paths(data_dir);
    ensure!(
        !p.node_crt.exists() && !p.ca_crt.exists() && !p.ca_key.exists(),
        "a farm identity already exists in {}",
        data_dir.display()
    );
    let ca_kp = KeyPair::generate_for(&PKCS_ED25519)?;
    let ca_cert_der = ca_params()?.self_signed(&ca_kp)?.der().to_vec();
    let node_kp = KeyPair::generate_for(&PKCS_ED25519)?;
    let mut id = Identity {
        node_id: NodeId(random_bytes()),
        node_name: node_name.to_string(),
        node_key_der: node_kp.serialize_der(),
        node_cert_der: Vec::new(),
        ca_cert_der,
        ca_key_der: Some(ca_kp.serialize_der()),
        blob_key: random_bytes(),
    };
    id.node_cert_der = sign_node_cert(&id, node_name, id.node_id, node_kp.public_key_raw())?;
    id.write_files(data_dir)?;
    Ok(id)
}

/// Load the identity from `data_dir`; `Ok(None)` when not joined to a farm.
pub fn load(data_dir: &Path) -> anyhow::Result<Option<Identity>> {
    let p = paths(data_dir);
    let Some(node_cert_der) = read_opt(&p.node_crt)? else {
        return Ok(None);
    };
    let node_id_hex = String::from_utf8(read_req(&p.node_id)?).context("node.id is not text")?;
    let id_bytes = data_encoding::HEXLOWER.decode(node_id_hex.trim().as_bytes()).context("node.id is not hex")?;
    let node_id = NodeId(id_bytes.try_into().map_err(|_| anyhow::anyhow!("node.id is not 16 bytes"))?);
    let blob_key: [u8; 32] = read_req(&p.blob_key)?.try_into().map_err(|_| anyhow::anyhow!("blob.key is not 32 bytes"))?;
    let id = Identity {
        node_id,
        node_name: node_name_from_cert(&node_cert_der)?,
        node_key_der: read_req(&p.node_key)?,
        node_cert_der,
        ca_cert_der: read_req(&p.ca_crt)?,
        ca_key_der: read_opt(&p.ca_key)?,
        blob_key,
    };
    ensure!(node_id_from_cert(&id.node_cert_der)? == id.node_id, "node.id does not match the node certificate");
    Ok(Some(id))
}

// ---------------------------------------------------------------- join code

/// A parsed join code `cf1-<token>-<fp>@<host>:<port>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinCode {
    pub token: [u8; 16],
    /// First 16 bytes of SHA-256 of the inviter's node cert DER.
    pub cert_fp: [u8; 16],
    pub addr: String,
}

fn b32(bytes: &[u8]) -> String {
    data_encoding::BASE32_NOPAD.encode(bytes).to_ascii_lowercase()
}

fn unb32_16(s: &str, what: &str) -> anyhow::Result<[u8; 16]> {
    let bytes = data_encoding::BASE32_NOPAD
        .decode(s.to_ascii_uppercase().as_bytes())
        .with_context(|| format!("join code {what} is not base32"))?;
    bytes.try_into().map_err(|_| anyhow::anyhow!("join code {what} has the wrong length"))
}

impl JoinCode {
    pub fn encode(&self) -> String {
        format!("cf1-{}-{}@{}", b32(&self.token), b32(&self.cert_fp), self.addr)
    }

    pub fn parse(s: &str) -> anyhow::Result<JoinCode> {
        let s = s.trim();
        let rest = s.strip_prefix("cf1-").context("not a crabfarm join code (expected prefix cf1-)")?;
        let (head, addr) = rest.split_once('@').context("join code has no @address")?;
        let (token, fp) = head.split_once('-').context("join code has no fingerprint")?;
        let (host, port) = addr.rsplit_once(':').context("join code address needs host:port")?;
        ensure!(!host.is_empty(), "join code host is empty");
        ensure!(!addr.chars().any(|c| c.is_whitespace() || c == '@'), "join code address contains invalid characters");
        ensure!(port.parse::<u16>().is_ok_and(|p| p != 0), "join code port is invalid");
        Ok(JoinCode { token: unb32_16(token, "token")?, cert_fp: unb32_16(fp, "fingerprint")?, addr: addr.to_string() })
    }
}

// ---------------------------------------------------------------- invites

#[derive(Serialize, Deserialize, Clone, Debug)]
struct InviteEntry {
    /// SHA-256 of the token, hex: the file never holds a redeemable token.
    hash: String,
    /// Unix seconds.
    expires: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct InviteFile {
    #[serde(default)]
    invites: Vec<InviteEntry>,
}

/// Pending invites held by the inviting node in memory (and in
/// `farm/invites.toml` so `crabfarm farm invite` can talk to a running node:
/// the CLI writes the invite file, the node reads it on each join attempt).
/// Read-modify-write is atomic per file replace; two processes redeeming the
/// same token at the same instant could both succeed, which needs the
/// 128-bit token and is accepted for P1 (the node serializes its own joins).
#[derive(Default)]
pub struct Invites {
    entries: Vec<InviteEntry>,
}

fn invite_path(data_dir: &Path) -> PathBuf {
    data_dir.join("farm").join("invites.toml")
}

fn token_hash(token: &[u8; 16]) -> [u8; 32] {
    Sha256::digest(token).into()
}

impl Invites {
    fn reload(&mut self, data_dir: &Path, now: u64) -> anyhow::Result<()> {
        let path = invite_path(data_dir);
        let file: InviteFile = match read_opt(&path)? {
            Some(b) => toml::from_str(std::str::from_utf8(&b).context("invites.toml is not UTF-8")?)
                .with_context(|| format!("parse {}", path.display()))?,
            None => InviteFile::default(),
        };
        self.entries = file.invites;
        // Expired invites are dropped here, so no later code path can redeem one.
        self.entries.retain(|e| e.expires > now);
        Ok(())
    }

    fn save(&self, data_dir: &Path) -> anyhow::Result<()> {
        let text = toml::to_string(&InviteFile { invites: self.entries.clone() })?;
        write_private(&invite_path(data_dir), text.as_bytes())
    }

    pub fn load(data_dir: &Path) -> anyhow::Result<Invites> {
        let mut i = Invites::default();
        i.reload(data_dir, unix_now())?;
        Ok(i)
    }

    /// Create a single-use token valid for 10 minutes and persist it.
    pub fn issue(&mut self, data_dir: &Path) -> anyhow::Result<[u8; 16]> {
        self.issue_at(data_dir, unix_now())
    }

    pub(crate) fn issue_at(&mut self, data_dir: &Path, now: u64) -> anyhow::Result<[u8; 16]> {
        self.reload(data_dir, now)?;
        ensure!(self.entries.len() < MAX_PENDING_INVITES, "too many pending invites");
        let token: [u8; 16] = random_bytes();
        self.entries.push(InviteEntry { hash: hex(&token_hash(&token)), expires: now + INVITE_TTL_SECS });
        self.save(data_dir)?;
        Ok(token)
    }

    /// Constant-time check; consumes the token on success. Reloads from disk first.
    pub fn redeem(&mut self, data_dir: &Path, token: &[u8; 16]) -> anyhow::Result<bool> {
        self.redeem_at(data_dir, token, unix_now())
    }

    pub(crate) fn redeem_at(&mut self, data_dir: &Path, token: &[u8; 16], now: u64) -> anyhow::Result<bool> {
        self.reload(data_dir, now)?;
        let want = hex(&token_hash(token));
        // Compare against every entry without early exit so timing does not
        // reveal which entry (if any) matched.
        let mut hit: Option<usize> = None;
        for (i, e) in self.entries.iter().enumerate() {
            if bool::from(e.hash.as_bytes().ct_eq(want.as_bytes())) {
                hit = Some(i);
            }
        }
        let Some(i) = hit else { return Ok(false) };
        self.entries.remove(i);
        self.save(data_dir)?;
        Ok(true)
    }
}

// ---------------------------------------------------------------- join: inviter

/// Inviter side of a join: validate the request, consume the token, sign.
/// Returns `JoinAccepted`, or an `Error` message (deliberately uniform for
/// bad/used/expired tokens so the answer is no oracle).
pub fn handle_join(id: &Identity, invites: &mut Invites, data_dir: &Path, request: &Message) -> anyhow::Result<Message> {
    let Message::JoinRequest { token, node_name, public_key } = request else {
        return Ok(Message::Error { message: "expected a join request".into() });
    };
    if id.ca_key_der.is_none() {
        return Ok(Message::Error { message: "this node cannot sign certificates".into() });
    }
    // Validate the request shape before burning the single-use token.
    if validate_node_name(node_name).is_err() || public_key.len() != 32 {
        return Ok(Message::Error { message: "invalid join request".into() });
    }
    if !invites.redeem(data_dir, token)? {
        return Ok(Message::Error { message: "invalid or expired invite".into() });
    }
    let known_peers = crate::discovery::PeerBook::load(data_dir)?.peers;
    // The CA picks the id: a joiner must not be able to ask for a certificate
    // carrying an existing member's id.
    let node_id = loop {
        let candidate = NodeId(random_bytes());
        if candidate != id.node_id && !known_peers.iter().any(|p| p.node_id == candidate) {
            break candidate;
        }
    };
    let node_cert_der = sign_node_cert(id, node_name, node_id, public_key)?;
    Ok(Message::JoinAccepted { node_id, node_cert_der, ca_cert_der: id.ca_cert_der.clone(), known_peers })
}

// ---------------------------------------------------------------- join: joiner

/// Check a `JoinAccepted` against what we asked for, then write the identity.
/// The CA cert comes from the pinned inviter, so the checks here are about
/// consistency: the certificate must be for our key and node id, issued by
/// the CA we were handed, and the CA must be a CA.
pub(crate) fn install_join_response(
    data_dir: &Path,
    node_name: &str,
    node_id: NodeId,
    node_kp: &KeyPair,
    node_cert_der: Vec<u8>,
    ca_cert_der: Vec<u8>,
) -> anyhow::Result<Identity> {
    {
        let (_, ca) = x509_parser::parse_x509_certificate(&ca_cert_der).map_err(|e| anyhow::anyhow!("parse CA certificate: {e}"))?;
        let (_, node) =
            x509_parser::parse_x509_certificate(&node_cert_der).map_err(|e| anyhow::anyhow!("parse node certificate: {e}"))?;
        ensure!(ca.is_ca(), "CA certificate is not a CA");
        ca.verify_signature(None).map_err(|_| anyhow::anyhow!("CA certificate is not self-signed"))?;
        node.verify_signature(Some(ca.public_key())).map_err(|_| anyhow::anyhow!("node certificate is not signed by the CA"))?;
        ensure!(
            node.public_key().subject_public_key.data.as_ref() == node_kp.public_key_raw(),
            "node certificate is for a different key"
        );
    }
    ensure!(node_id_from_cert(&node_cert_der)? == node_id, "node certificate is for a different node id");
    let id = Identity {
        node_id,
        node_name: node_name.to_string(),
        node_key_der: node_kp.serialize_der(),
        node_cert_der,
        ca_cert_der,
        ca_key_der: None,
        blob_key: random_bytes(),
    };
    id.write_files(data_dir)?;
    Ok(id)
}

/// Joiner side: connect to `code.addr`, pin the inviter cert by fingerprint,
/// send `JoinRequest`, receive `JoinAccepted`, write identity files. Returns
/// the identity and the peers the inviter knows (the inviter itself included).
pub async fn join(data_dir: &Path, node_name: &str, code: &JoinCode) -> anyhow::Result<(Identity, Vec<PeerInfo>)> {
    use crate::transport::FramedConn;
    validate_node_name(node_name)?;
    let p = paths(data_dir);
    ensure!(!p.node_crt.exists() && !p.ca_crt.exists(), "this machine already has a farm identity in {}", data_dir.display());

    let node_kp = KeyPair::generate_for(&PKCS_ED25519)?;
    let cfg = crate::transport::join_client_config(code.cert_fp)?;
    let connect = async {
        let tcp = tokio::net::TcpStream::connect(&code.addr).await.with_context(|| format!("connect to {}", code.addr))?;
        tcp.set_nodelay(true).ok();
        let name = ServerName::try_from("crabfarm.local").context("server name")?;
        tokio_rustls::TlsConnector::from(cfg)
            .connect(name, tcp)
            .await
            .context("TLS handshake (is the join code's fingerprint right?)")
    };
    let stream = tokio::time::timeout(Duration::from_secs(5), connect).await.context("join connect timed out")??;
    let inviter_cert =
        stream.get_ref().1.peer_certificates().and_then(|c| c.first()).map(|c| c.to_vec()).context("inviter sent no certificate")?;
    let inviter_id = node_id_from_cert(&inviter_cert)?;
    let inviter_name = node_name_from_cert(&inviter_cert).unwrap_or_default();

    let mut conn = FramedConn::new(stream);
    conn.send(&Message::JoinRequest {
        token: code.token,
        node_name: node_name.to_string(),
        public_key: node_kp.public_key_raw().to_vec(),
    })
    .await?;
    let reply = tokio::time::timeout(Duration::from_secs(10), conn.recv()).await.context("join reply timed out")??;
    let (node_id, node_cert_der, ca_cert_der, mut known_peers) = match reply {
        Some(Message::JoinAccepted { node_id, node_cert_der, ca_cert_der, known_peers }) => {
            (node_id, node_cert_der, ca_cert_der, known_peers)
        }
        Some(Message::Error { message }) => bail!("join refused: {message}"),
        Some(_) => bail!("unexpected reply to join request (this node speaks protocol {PROTO_VERSION})"),
        None => bail!("inviter closed the connection"),
    };
    let id = install_join_response(data_dir, node_name, node_id, &node_kp, node_cert_der, ca_cert_der)?;
    known_peers.retain(|p| p.node_id != inviter_id && p.node_id != id.node_id);
    known_peers.push(PeerInfo { node_id: inviter_id, node_name: inviter_name, addrs: vec![code.addr.clone()], last_seen: unix_now() });
    Ok((id, known_peers))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> JoinCode {
        JoinCode { token: [7; 16], cert_fp: [0xab; 16], addr: "192.168.1.5:47070".into() }
    }

    #[test]
    fn join_code_round_trip() {
        let c = sample();
        let s = c.encode();
        assert!(s.starts_with("cf1-"));
        assert_eq!(s.len(), 4 + 26 + 1 + 26 + 1 + "192.168.1.5:47070".len());
        assert_eq!(s, s.to_ascii_lowercase());
        assert_eq!(JoinCode::parse(&s).unwrap(), c);
        assert_eq!(JoinCode::parse(&format!("  {s}\n")).unwrap(), c);
        let v6 = JoinCode { addr: "[fe80::1]:47070".into(), ..sample() };
        assert_eq!(JoinCode::parse(&v6.encode()).unwrap(), v6);
        let named = JoinCode { addr: "desk.tailnet:1".into(), ..sample() };
        assert_eq!(JoinCode::parse(&named.encode()).unwrap(), named);
    }

    #[test]
    fn join_code_exact_format() {
        // Pins the documented layout: base32 lowercase, no padding.
        let c = JoinCode { token: [0; 16], cert_fp: [0xff; 16], addr: "h:1".into() };
        assert_eq!(c.encode(), "cf1-aaaaaaaaaaaaaaaaaaaaaaaaaa-77777777777777777777777774@h:1");
    }

    #[test]
    fn join_code_malformed() {
        let good = sample().encode();
        let (head, addr) = good.split_once('@').unwrap();
        let fp = head.rsplit('-').next().unwrap();
        let tok = head.split('-').nth(1).unwrap();
        let bad = [
            String::new(),
            "cf2-aaaa-bbbb@h:1".to_string(),
            good.replacen("cf1-", "cf0-", 1),
            head.to_string(),
            format!("{head}@"),
            format!("{head}@host"),
            format!("{head}@:47070"),
            format!("{head}@host:0"),
            format!("{head}@host:70000"),
            format!("{head}@host:x"),
            format!("{head}@ho st:1"),
            format!("cf1-{tok}@{addr}"),                  // no fingerprint
            format!("cf1-{tok}1-{fp}@{addr}"),            // '1' is not in the base32 alphabet
            format!("cf1-{}-{fp}@{addr}", &tok[..20]),    // short token
            format!("cf1-{tok}-{}@{addr}", &fp[..20]),    // short fingerprint
            format!("cf1-{tok}a-{fp}@{addr}"),            // long token
            format!("{good}@x:1"),
        ];
        for s in bad {
            assert!(JoinCode::parse(&s).is_err(), "should reject {s:?}");
        }
    }

    #[test]
    fn create_load_and_cert_shape() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_none());
        let id = create(dir.path(), "alpha").unwrap();
        assert!(create(dir.path(), "again").is_err(), "must not overwrite an existing farm");
        let back = load(dir.path()).unwrap().unwrap();
        assert_eq!(back.node_id, id.node_id);
        assert_eq!(back.node_name, "alpha");
        assert_eq!(back.short_farm_id().len(), 32);
        assert!(back.ca_key_der.is_some());
        assert_eq!(node_id_from_cert(&back.node_cert_der).unwrap(), id.node_id);
        assert!(node_id_from_cert(&back.ca_cert_der).is_err(), "CA has no node id");
        let dbg = format!("{back:?}");
        assert!(!dbg.contains("node_key") && !dbg.contains("blob_key"));

        use x509_parser::extensions::ParsedExtension;
        let (_, cert) = x509_parser::parse_x509_certificate(&back.node_cert_der).unwrap();
        assert!(!cert.is_ca());
        let span = cert.validity().not_after.timestamp() - cert.validity().not_before.timestamp();
        assert!(span > 9 * 365 * 24 * 3600, "about ten years");
        let ekus = cert.extensions().iter().find_map(|e| match e.parsed_extension() {
            ParsedExtension::ExtendedKeyUsage(k) => Some(k.clone()),
            _ => None,
        });
        let ekus = ekus.unwrap();
        assert!(ekus.server_auth && ekus.client_auth);
        let (_, ca) = x509_parser::parse_x509_certificate(&back.ca_cert_der).unwrap();
        assert!(ca.is_ca());
        assert_eq!(ca.basic_constraints().unwrap().unwrap().value.path_len_constraint, Some(0));
        assert_eq!(ca.public_key().algorithm.algorithm.to_id_string(), "1.3.101.112", "Ed25519 CA");
        assert!(cert.verify_signature(Some(ca.public_key())).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn key_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        create(dir.path(), "alpha").unwrap();
        for f in ["farm/ca.key", "node.key", "blob.key"] {
            let mode = std::fs::metadata(dir.path().join(f)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{f}");
        }
    }

    #[test]
    fn invite_token_single_use() {
        let dir = tempfile::tempdir().unwrap();
        let mut inv = Invites::load(dir.path()).unwrap();
        let t = inv.issue(dir.path()).unwrap();
        // A second process (the node) sees the CLI's invite through the file.
        let mut node = Invites::load(dir.path()).unwrap();
        assert!(!node.redeem(dir.path(), &[0u8; 16]).unwrap(), "wrong token");
        assert!(node.redeem(dir.path(), &t).unwrap());
        assert!(!node.redeem(dir.path(), &t).unwrap(), "reused token");
        assert!(!Invites::load(dir.path()).unwrap().redeem(dir.path(), &t).unwrap(), "reuse across processes");
    }

    #[test]
    fn invite_token_expires() {
        let dir = tempfile::tempdir().unwrap();
        let mut inv = Invites::default();
        let t0 = 1_000_000;
        let t = inv.issue_at(dir.path(), t0).unwrap();
        assert!(!inv.redeem_at(dir.path(), &t, t0 + INVITE_TTL_SECS).unwrap(), "expired at exactly the TTL");
        let t2 = inv.issue_at(dir.path(), t0).unwrap();
        assert!(inv.redeem_at(dir.path(), &t2, t0 + INVITE_TTL_SECS - 1).unwrap(), "valid just before");
    }

    #[test]
    fn invite_file_holds_no_redeemable_token() {
        let dir = tempfile::tempdir().unwrap();
        let mut inv = Invites::default();
        let t = inv.issue(dir.path()).unwrap();
        let text = std::fs::read_to_string(invite_path(dir.path())).unwrap();
        assert!(!text.contains(&hex(&t)) && !text.contains(&b32(&t)));
    }

    #[test]
    fn handle_join_requires_ca_and_valid_request() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let ca_node = create(a.path(), "ca").unwrap();
        let mut inv = Invites::default();
        let token = inv.issue(a.path()).unwrap();
        let kp = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        let pk = kp.public_key_raw().to_vec();
        let req = |token: [u8; 16], pk: Vec<u8>, name: &str| Message::JoinRequest {
            token,
            node_name: name.into(),
            public_key: pk,
        };
        // Bad key length / name are refused and do not burn the token.
        for bad in [req(token, vec![1; 31], "x"), req(token, pk.clone(), ""), req(token, pk.clone(), "a\nb")] {
            assert!(matches!(handle_join(&ca_node, &mut inv, a.path(), &bad).unwrap(), Message::Error { .. }));
        }
        // A joined node (no CA key) cannot sign even with a valid token.
        let t = inv.issue(a.path()).unwrap();
        let Message::JoinAccepted { node_id, node_cert_der, ca_cert_der, .. } =
            handle_join(&ca_node, &mut inv, a.path(), &req(t, pk.clone(), "beta")).unwrap()
        else {
            panic!("expected JoinAccepted")
        };
        let joined = install_join_response(b.path(), "beta", node_id, &kp, node_cert_der, ca_cert_der).unwrap();
        let t = inv.issue(b.path()).unwrap();
        let reply = handle_join(&joined, &mut inv, b.path(), &req(t, pk.clone(), "gamma")).unwrap();
        assert!(matches!(reply, Message::Error { .. }));
        // The original token still works (nothing above consumed it).
        let ok = handle_join(&ca_node, &mut inv, a.path(), &req(token, pk.clone(), "delta")).unwrap();
        assert!(matches!(ok, Message::JoinAccepted { .. }));
        // Not a join request at all.
        assert!(matches!(handle_join(&ca_node, &mut inv, a.path(), &Message::Started).unwrap(), Message::Error { .. }));
    }

    #[test]
    fn ca_chooses_the_node_id() {
        let a = tempfile::tempdir().unwrap();
        let ca_node = create(a.path(), "ca").unwrap();
        let mut inv = Invites::default();
        let kp = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        let mut seen = Vec::new();
        for i in 0..3 {
            let token = inv.issue(a.path()).unwrap();
            let req = Message::JoinRequest { token, node_name: format!("n{i}"), public_key: kp.public_key_raw().to_vec() };
            let Message::JoinAccepted { node_id, node_cert_der, .. } = handle_join(&ca_node, &mut inv, a.path(), &req).unwrap() else {
                panic!("expected JoinAccepted")
            };
            // The certificate carries exactly the id the CA reported, and it is
            // nobody else's: not the CA's, not an earlier joiner's, not a value
            // a joiner could have predicted (the old requests used [9; 16]).
            assert_eq!(node_id_from_cert(&node_cert_der).unwrap(), node_id);
            assert_ne!(node_id, ca_node.node_id);
            assert_ne!(node_id, NodeId([9; 16]));
            assert!(!seen.contains(&node_id));
            seen.push(node_id);
        }
    }

    #[test]
    fn joiner_rejects_inconsistent_response() {
        let a = tempfile::tempdir().unwrap();
        let ca_node = create(a.path(), "ca").unwrap();
        let mine = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        let other = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        let nid = NodeId([5; 16]);
        let ok_cert = sign_node_cert(&ca_node, "me", nid, mine.public_key_raw()).unwrap();
        let try_install = |cert: Vec<u8>, ca: Vec<u8>, nid: NodeId, kp: &KeyPair| {
            let d = tempfile::tempdir().unwrap();
            let r = install_join_response(d.path(), "me", nid, kp, cert, ca);
            (r.is_ok(), load(d.path()).unwrap().is_some())
        };
        assert_eq!(try_install(ok_cert.clone(), ca_node.ca_cert_der.clone(), nid, &mine), (true, true));
        // Certificate for somebody else's key.
        assert_eq!(try_install(ok_cert.clone(), ca_node.ca_cert_der.clone(), nid, &other), (false, false));
        // Certificate for another node id.
        assert_eq!(try_install(ok_cert.clone(), ca_node.ca_cert_der.clone(), NodeId([6; 16]), &mine), (false, false));
        // Certificate from a different CA than the one handed over.
        let b = tempfile::tempdir().unwrap();
        let foreign = create(b.path(), "foreign").unwrap();
        assert_eq!(try_install(ok_cert, foreign.ca_cert_der.clone(), nid, &mine), (false, false));
    }
}
