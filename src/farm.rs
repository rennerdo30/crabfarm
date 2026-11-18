//! Package B. Farm identity: CA, node certificates, invites and joining
//! (DESIGN section 3).

use crate::proto::{NodeId, PeerInfo};
use std::path::Path;

/// Loaded identity of this node.
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

impl Identity {
    /// First 16 bytes of SHA-256(CA cert DER), lowercase hex.
    pub fn short_farm_id(&self) -> String {
        todo!("package B")
    }
}

/// Create a new farm in `data_dir` (fails if one exists): CA key + cert,
/// node key + cert signed by the CA, node id, blob key. Files owner-only.
pub fn create(data_dir: &Path, node_name: &str) -> anyhow::Result<Identity> {
    let _ = (data_dir, node_name);
    todo!("package B")
}

/// Load the identity from `data_dir`; `Ok(None)` when not joined to a farm.
pub fn load(data_dir: &Path) -> anyhow::Result<Option<Identity>> {
    let _ = data_dir;
    todo!("package B")
}

/// A parsed join code `cf1-<token>-<fp>@<host>:<port>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinCode {
    pub token: [u8; 16],
    /// First 16 bytes of SHA-256 of the inviter's node cert DER.
    pub cert_fp: [u8; 16],
    pub addr: String,
}

impl JoinCode {
    pub fn encode(&self) -> String {
        todo!("package B")
    }
    pub fn parse(s: &str) -> anyhow::Result<JoinCode> {
        let _ = s;
        todo!("package B")
    }
}

/// Pending invites held by the inviting node in memory (and in
/// `farm/invites.toml` so `crabfarm farm invite` can talk to a running node:
/// the CLI writes the invite file, the node reads it on each join attempt).
pub struct Invites {
    // implementation-defined
}

impl Invites {
    pub fn load(data_dir: &Path) -> anyhow::Result<Invites> {
        let _ = data_dir;
        todo!("package B")
    }
    /// Create a single-use token valid for 10 minutes and persist it.
    pub fn issue(&mut self, data_dir: &Path) -> anyhow::Result<[u8; 16]> {
        let _ = data_dir;
        todo!("package B")
    }
    /// Constant-time check; consumes the token on success. Reloads from disk first.
    pub fn redeem(&mut self, data_dir: &Path, token: &[u8; 16]) -> anyhow::Result<bool> {
        let _ = (data_dir, token);
        todo!("package B")
    }
}

/// CA side: sign a node certificate for `public_key` (raw Ed25519, 32 bytes).
/// CN = node_name, SAN URI `crabfarm:node:<node id hex>`. 10-year validity.
pub fn sign_node_cert(id: &Identity, node_name: &str, node_id: NodeId, public_key: &[u8]) -> anyhow::Result<Vec<u8>> {
    let _ = (id, node_name, node_id, public_key);
    todo!("package B")
}

/// Extract the node id from a peer certificate (SAN URI).
pub fn node_id_from_cert(cert_der: &[u8]) -> anyhow::Result<NodeId> {
    let _ = cert_der;
    todo!("package B")
}

/// Joiner side: connect to `code.addr`, pin the inviter cert by fingerprint,
/// send `JoinRequest`, receive `JoinAccepted`, write identity files. Returns
/// the identity and the peers the inviter knows.
pub async fn join(data_dir: &Path, node_name: &str, code: &JoinCode) -> anyhow::Result<(Identity, Vec<PeerInfo>)> {
    let _ = (data_dir, node_name, code);
    todo!("package B")
}
