//! Local IPC between wrappers / CLI commands and the node (DESIGN section 3):
//! plain TCP on `127.0.0.1` with a random token. The port is in `ipc.port`,
//! the token in `ipc.token` (both in the data dir, rewritten at every node
//! start). The first frame must be `IpcAuth`.

use crate::proto::Message;
use crate::transport::FramedConn;
use anyhow::Context;
use std::path::{Path, PathBuf};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::net::{TcpListener, TcpStream};

/// A wrapper must not slow a build down when the node is gone.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const AUTH_TIMEOUT: Duration = Duration::from_secs(2);
/// `IpcAuth` is 33 bytes; nothing bigger is accepted before authentication.
const AUTH_MAX_FRAME: usize = 256;

pub fn port_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ipc.port")
}

pub fn token_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ipc.token")
}

/// Write a file other users must not read (Unix: 0600 from creation; Windows
/// inherits the per-user data directory ACL).
pub fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    f.write_all(data)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Server side: bind a loopback port, generate a fresh token and publish both.
/// The old port file is removed first so a wrapper never reads a stale pair.
pub async fn bind(data_dir: &Path) -> anyhow::Result<(TcpListener, [u8; 32])> {
    let _ = std::fs::remove_file(port_path(data_dir));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind IPC port")?;
    let port = listener.local_addr().context("IPC address")?.port();
    let mut token = [0u8; 32];
    rand::fill(&mut token[..]);
    write_private(&token_path(data_dir), &token)?;
    // Written last: its presence means the token is already in place.
    write_private(&port_path(data_dir), port.to_string().as_bytes())?;
    Ok((listener, token))
}

/// Remove the published port (node shutdown) so wrappers skip the connect.
pub fn unpublish(data_dir: &Path) {
    let _ = std::fs::remove_file(port_path(data_dir));
}

/// Server side: the first frame must be `IpcAuth` with the right token.
pub async fn authenticate(conn: &mut FramedConn<TcpStream>, token: &[u8; 32]) -> bool {
    let first = tokio::time::timeout(AUTH_TIMEOUT, conn.recv_max(AUTH_MAX_FRAME)).await;
    match first {
        Ok(Ok(Some(Message::IpcAuth { token: got }))) => bool::from(got.ct_eq(token)),
        _ => false,
    }
}

/// Client side: connect within 200 ms and authenticate. `Err` means "no node
/// reachable"; wrappers then run rustc locally.
pub async fn connect(data_dir: &Path) -> anyhow::Result<FramedConn<TcpStream>> {
    let port: u16 = std::fs::read_to_string(port_path(data_dir))
        .context("no ipc.port (node not running)")?
        .trim()
        .parse()
        .context("ipc.port is not a port number")?;
    let token: [u8; 32] = std::fs::read(token_path(data_dir))
        .context("no ipc.token")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("ipc.token is not 32 bytes"))?;
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(("127.0.0.1", port)))
        .await
        .context("IPC connect timed out")?
        .context("IPC connect refused")?;
    tcp.set_nodelay(true).ok();
    let mut conn = FramedConn::new(tcp);
    conn.send(&Message::IpcAuth { token }).await?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wrong_token_is_refused_and_right_token_accepted() {
        let d = tempfile::tempdir().unwrap();
        let (listener, token) = bind(d.path()).await.unwrap();
        let server = tokio::spawn(async move {
            let mut results = Vec::new();
            for _ in 0..2 {
                let (s, _) = listener.accept().await.unwrap();
                let mut c = FramedConn::new(s);
                results.push(authenticate(&mut c, &token).await);
            }
            results
        });
        // Right token through the real client path.
        let _ok = connect(d.path()).await.unwrap();
        // Wrong token by hand.
        let port: u16 = std::fs::read_to_string(port_path(d.path()))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut bad = FramedConn::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        bad.send(&Message::IpcAuth { token: [0; 32] })
            .await
            .unwrap();
        assert_eq!(server.await.unwrap(), vec![true, false]);
    }

    #[tokio::test]
    async fn first_frame_other_than_auth_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (listener, token) = bind(d.path()).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            authenticate(&mut FramedConn::new(s), &token).await
        });
        let mut c = FramedConn::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        c.send(&Message::QueryStatus).await.unwrap();
        assert!(!server.await.unwrap());
    }

    #[tokio::test]
    async fn connect_fails_fast_without_a_node() {
        let d = tempfile::tempdir().unwrap();
        assert!(connect(d.path()).await.is_err());
    }
}
