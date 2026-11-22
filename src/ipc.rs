//! Local IPC between wrappers / CLI commands and the node (DESIGN section 3):
//! plain TCP on `127.0.0.1` with a random token. The port is in `ipc.port`,
//! the token in `ipc.token` (both in the data dir, rewritten at every node
//! start). The first frame must be `IpcAuth`.

use crate::proto::{Message, PROTO_VERSION};
use crate::transport::FramedConn;
use anyhow::Context;
use std::path::{Path, PathBuf};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::net::{TcpListener, TcpStream};

/// A wrapper must not slow a build down when the node is gone.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const AUTH_TIMEOUT: Duration = Duration::from_secs(2);
/// The node sends `Message::Keepalive` this often while a `Submit` is in flight.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
/// A wrapper that heard nothing from the node for this long runs rustc itself
/// (a node that is hung or gone must not hang the build).
pub const NODE_SILENCE_LIMIT: Duration = Duration::from_secs(30);
/// `IpcAuth` is 34 bytes; nothing bigger is accepted before authentication.
const AUTH_MAX_FRAME: usize = 256;

pub fn port_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ipc.port")
}

pub fn token_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ipc.token")
}

/// How long one wrapper's finding that the node is unresponsive makes later
/// wrappers skip the node (CF-25).
pub const UNRESPONSIVE_TTL: Duration = Duration::from_secs(180);

/// The marker a wrapper writes after the node stayed silent for
/// [`NODE_SILENCE_LIMIT`]: its content is the unix second it expires.
pub fn unresponsive_path(data_dir: &Path) -> PathBuf {
    data_dir.join("node-unresponsive")
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record that the node did not answer (atomic: a temporary file renamed over
/// the marker, so a reader never sees half of it). Best effort.
pub fn mark_unresponsive(data_dir: &Path) {
    let path = unresponsive_path(data_dir);
    let tmp = path.with_extension("tmp");
    let until = unix_secs() + UNRESPONSIVE_TTL.as_secs();
    if std::fs::write(&tmp, until.to_string()).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// When the marker expires (unix seconds), or `None` when there is none, it is
/// expired, or its content is not a plausible expiry (a leftover or a
/// hand-written file must not switch the farm off for long).
pub fn unresponsive_until(data_dir: &Path) -> Option<u64> {
    unresponsive_until_at(data_dir, unix_secs())
}

fn unresponsive_until_at(data_dir: &Path, now: u64) -> Option<u64> {
    let text = std::fs::read_to_string(unresponsive_path(data_dir)).ok()?;
    let until: u64 = text.trim().parse().ok()?;
    (until > now && until <= now + UNRESPONSIVE_TTL.as_secs()).then_some(until)
}

/// The node answers: forget an earlier finding that it did not.
pub fn clear_unresponsive(data_dir: &Path) {
    let _ = std::fs::remove_file(unresponsive_path(data_dir));
}

/// Write a file other users must not read (see `secfile`).
pub fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    crate::secfile::write_private(path, data)
}

/// Server side: bind a loopback port, generate a fresh token and publish both.
/// The old port file is removed first so a wrapper never reads a stale pair.
pub async fn bind(data_dir: &Path) -> anyhow::Result<(TcpListener, [u8; 32])> {
    let _ = std::fs::remove_file(port_path(data_dir));
    // A node that starts is answering: whatever an earlier wrapper found is stale.
    clear_unresponsive(data_dir);
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

/// What the first frame of a local IPC connection said.
#[derive(Debug, PartialEq, Eq)]
pub enum Auth {
    /// The right token and the node's protocol version.
    Accepted,
    /// Not a valid `IpcAuth`, or the wrong token.
    Refused,
    /// The right token, spoken by a wrapper of another protocol version:
    /// `Some(version)`, or `None` for the frame older versions sent (no
    /// version in it). The node answers with an error and closes.
    OtherVersion(Option<u16>),
}

/// The text the node sends a wrapper of another protocol version.
pub fn version_error(theirs: Option<u16>) -> String {
    let them = theirs.map_or("an older protocol".to_string(), |v| format!("protocol {v}"));
    format!(
        "this node speaks protocol {PROTO_VERSION}, the wrapper speaks {them}: \
         update crabfarm so that the node and the wrapper are the same version"
    )
}

/// Server side: the first frame must be `IpcAuth` with the right token and
/// this node's protocol version. The token is checked first, so a process
/// that does not know it learns nothing, not even the version.
pub async fn authenticate(conn: &mut FramedConn<TcpStream>, token: &[u8; 32]) -> Auth {
    let first = tokio::time::timeout(AUTH_TIMEOUT, conn.recv_raw(AUTH_MAX_FRAME)).await;
    let Ok(Ok(Some(raw))) = first else {
        return Auth::Refused;
    };
    classify(&raw, token)
}

fn classify(raw: &[u8], token: &[u8; 32]) -> Auth {
    if let Ok(Message::IpcAuth { token: got, proto }) = postcard::from_bytes::<Message>(raw) {
        return if !bool::from(got.ct_eq(token)) {
            Auth::Refused
        } else if proto == PROTO_VERSION {
            Auth::Accepted
        } else {
            Auth::OtherVersion(Some(proto))
        };
    }
    // The frame of the versions before 6: the variant tag and the token, no
    // version. Its token is checked like any other.
    let tag = postcard::to_stdvec(&Message::IpcAuth {
        token: [0; 32],
        proto: 0,
    })
    .ok()
    .and_then(|b| b.first().copied());
    match (raw.split_first(), tag) {
        (Some((t, rest)), Some(tag)) if *t == tag && rest.len() == 32 => {
            if bool::from(rest.ct_eq(token)) {
                Auth::OtherVersion(None)
            } else {
                Auth::Refused
            }
        }
        _ => Auth::Refused,
    }
}

/// Outcome of reading the next IPC frame within a silence limit.
#[derive(Debug)]
pub enum Recv {
    Frame(Box<Message>),
    /// The node closed or reset the connection before sending a frame.
    Closed,
    /// No frame arrived within the silence limit (the node appears hung).
    TimedOut,
}

/// Client side: the next frame, distinguishing a closed connection from a
/// silence timeout (CF-44: only an actual timeout marks the node unresponsive).
/// The connection must not be used again after `Closed` or `TimedOut`.
pub async fn recv_outcome(conn: &mut FramedConn<TcpStream>, limit: Duration) -> Recv {
    match tokio::time::timeout(limit, conn.recv()).await {
        Ok(Ok(Some(m))) => Recv::Frame(Box::new(m)),
        Ok(_) => Recv::Closed,
        Err(_) => Recv::TimedOut,
    }
}

/// Client side: the next frame, or `None` when the node ended the connection,
/// broke the framing, or stayed silent for `limit` (a `Keepalive` counts as a
/// frame: callers skip it and call again). The connection must not be used
/// again after `None` (a timed-out read is not cancel-safe).
pub async fn recv_within(conn: &mut FramedConn<TcpStream>, limit: Duration) -> Option<Message> {
    match recv_outcome(conn, limit).await {
        Recv::Frame(m) => Some(*m),
        Recv::Closed | Recv::TimedOut => None,
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
    conn.send(&Message::IpcAuth {
        token,
        proto: PROTO_VERSION,
    })
    .await?;
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
        bad.send(&Message::IpcAuth {
            token: [0; 32],
            proto: PROTO_VERSION,
        })
        .await
        .unwrap();
        assert_eq!(server.await.unwrap(), vec![Auth::Accepted, Auth::Refused]);
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
        assert_eq!(server.await.unwrap(), Auth::Refused);
    }

    /// CF-28: a wrapper of another protocol version is told so, whether it
    /// sends a version (5) or the old frame without one, and never mistaken
    /// for a valid or an invalid token. A wrong token stays a plain refusal.
    #[test]
    fn a_wrapper_of_another_version_is_recognised_not_misread() {
        let token = [7u8; 32];
        let enc = |t: [u8; 32], proto: u16| {
            postcard::to_stdvec(&Message::IpcAuth { token: t, proto }).unwrap()
        };
        assert_eq!(classify(&enc(token, PROTO_VERSION), &token), Auth::Accepted);
        assert_eq!(
            classify(&enc(token, 5), &token),
            Auth::OtherVersion(Some(5))
        );
        assert_eq!(classify(&enc([8; 32], 5), &token), Auth::Refused);
        // The frame of protocol 5: tag + token, nothing else.
        let mut old = enc(token, 0);
        old.pop();
        assert_eq!(old.len(), 33);
        assert_eq!(classify(&old, &token), Auth::OtherVersion(None));
        old[1] ^= 1;
        assert_eq!(classify(&old, &token), Auth::Refused, "wrong token");
        for junk in [&[][..], &[1u8][..], &[0xFF; 40][..], &old[..20]] {
            assert_eq!(classify(junk, &token), Auth::Refused);
        }
        assert!(version_error(None).contains("update crabfarm"));
        assert!(version_error(Some(5)).contains("protocol 5"));
    }

    /// CF-25: the marker is short-lived, rejects implausible content, and goes
    /// away when the node answers.
    #[test]
    fn the_unresponsive_marker_expires_and_is_cleared() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(unresponsive_until(d.path()), None);
        mark_unresponsive(d.path());
        let until = unresponsive_until(d.path()).expect("fresh marker");
        assert!(until > unix_secs() && until <= unix_secs() + 180);
        assert!(
            !d.path().join("node-unresponsive.tmp").exists(),
            "no temporary file is left"
        );
        // Expired, absurdly far in the future, and not a number.
        let now = unix_secs();
        for (text, want) in [
            (format!("{}", now - 1), None),
            (format!("{}", now + 100_000), None),
            ("soon".to_string(), None),
            (format!("{}", now + 60), Some(now + 60)),
        ] {
            std::fs::write(unresponsive_path(d.path()), text).unwrap();
            assert_eq!(unresponsive_until_at(d.path(), now), want);
        }
        clear_unresponsive(d.path());
        assert_eq!(unresponsive_until(d.path()), None);
        clear_unresponsive(d.path());
    }

    #[tokio::test]
    async fn connect_fails_fast_without_a_node() {
        let d = tempfile::tempdir().unwrap();
        assert!(connect(d.path()).await.is_err());
    }

    /// CF-44: an immediately closed IPC connection returns `Recv::Closed`
    /// rather than `Recv::TimedOut`, so it does not trigger `node-unresponsive`.
    #[tokio::test]
    async fn recv_outcome_distinguishes_closed_from_timeout() {
        let d = tempfile::tempdir().unwrap();
        let (listener, _token) = bind(d.path()).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            drop(s);
            let (s2, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(s2);
        });
        let mut c1 = FramedConn::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        assert!(matches!(
            recv_outcome(&mut c1, Duration::from_secs(2)).await,
            Recv::Closed
        ));
        let mut c2 = FramedConn::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        assert!(matches!(
            recv_outcome(&mut c2, Duration::from_millis(80)).await,
            Recv::TimedOut
        ));
        accept.abort();
    }
}
