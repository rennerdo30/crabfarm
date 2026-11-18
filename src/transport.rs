//! Package B. Framing and mutual TLS 1.3 (DESIGN sections 3 and 11).

use crate::farm::Identity;
use crate::proto::{Message, NodeId};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

/// Length-prefixed postcard framing over any async stream.
pub struct FramedConn<S> {
    pub stream: S,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> FramedConn<S> {
    pub fn new(stream: S) -> Self {
        FramedConn { stream }
    }
    /// Encode and write one frame (u32 LE length + postcard), flush.
    pub async fn send(&mut self, msg: &Message) -> anyhow::Result<()> {
        let _ = msg;
        todo!("package B")
    }
    /// Read one frame; `Ok(None)` on clean EOF; error above MAX_FRAME.
    pub async fn recv(&mut self) -> anyhow::Result<Option<Message>> {
        todo!("package B")
    }
}

/// rustls configs for the farm: TLS 1.3 only, ring provider, both sides
/// present certs that must be signed by the farm CA (pinned by DER).
pub struct TlsContext {
    pub server: Arc<tokio_rustls::rustls::ServerConfig>,
    pub client: Arc<tokio_rustls::rustls::ClientConfig>,
}

impl TlsContext {
    pub fn new(id: &Identity) -> anyhow::Result<TlsContext> {
        let _ = id;
        todo!("package B")
    }
}

pub type ClientStream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;
pub type ServerStream = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;

/// Connect to a peer (`host:port`), TLS handshake, exchange `Hello`.
/// Timeout 5 s for connect + handshake. Returns the peer's node id (from its
/// certificate, checked against its Hello) and the framed stream.
pub async fn connect(
    tls: &TlsContext,
    id: &Identity,
    addr: &str,
) -> anyhow::Result<(NodeId, Message, FramedConn<ClientStream>)> {
    let _ = (tls, id, addr);
    todo!("package B")
}

/// Server side of `connect` for an accepted TCP stream.
pub async fn accept(
    tls: &TlsContext,
    id: &Identity,
    tcp: tokio::net::TcpStream,
) -> anyhow::Result<(NodeId, Message, FramedConn<ServerStream>)> {
    let _ = (tls, id, tcp);
    todo!("package B")
}

/// Join handshake helpers: a server config that accepts clients WITHOUT a
/// certificate (only for the join listener path: the node accepts a join on
/// the normal port by peeking the first frame), and a client config that pins
/// the server cert by SHA-256 fingerprint prefix instead of the CA.
pub fn join_client_config(cert_fp: [u8; 16]) -> anyhow::Result<Arc<tokio_rustls::rustls::ClientConfig>> {
    let _ = cert_fp;
    todo!("package B")
}

/// Server config used for every inbound connection: client certs are
/// *optional* at the TLS layer; `accept` then requires a valid farm cert for
/// everything except a `JoinRequest` first frame.
pub fn server_config_optional_client_auth(id: &Identity) -> anyhow::Result<Arc<tokio_rustls::rustls::ServerConfig>> {
    let _ = id;
    todo!("package B")
}
