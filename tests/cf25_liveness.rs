//! CF-25: a peer that goes silent in the middle of a job (lid closed, Wi-Fi
//! gone, no reset) must not stall the build. The real node and the real
//! wrapper run as child processes; the peer is a fake farm member in this
//! process that accepts the job and then says nothing. Loopback only, mDNS off.

mod common;

use common::{make_crate, run_wrapper};
use crabfarm::farm::{self, JoinCode};
use crabfarm::proto::{Message, Status, ToolchainId};
use crabfarm::transport::{self, Accepted, TlsContext};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn crabfarm(home: &Path) -> Command {
    let mut c = Command::new(common::BIN);
    c.env("CRABFARM_HOME", home).env_remove("CRABFARM_LOG");
    c
}

fn run_ok(mut c: Command, what: &str) -> String {
    let out = c
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("{what}: {e}"));
    assert!(
        out.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The compiler the wrapper will use, as the peers would announce it.
fn local_toolchain(dir: &Path) -> ToolchainId {
    let out = Command::new("rustc")
        .arg("-vV")
        .current_dir(dir)
        .output()
        .expect("rustc -vV");
    crabfarm::toolchain::parse_version_verbose(&String::from_utf8_lossy(&out.stdout))
        .expect("parse rustc -vV")
}

/// What the fake peer does with a job it accepted.
#[derive(Clone, Copy)]
enum Peer {
    /// Take the `JobRequest`, say `Started`, then never send another byte and
    /// keep the connection open.
    SilentAfterStart,
}

struct FakePeer {
    job_seen: Arc<AtomicBool>,
}

/// Start a real node in `home` that has the fake peer as its static peer, and
/// make the fake peer a member of that node's farm.
async fn start_farm(
    tmp: &Path,
    behavior: Peer,
) -> (Proc, FakePeer, std::path::PathBuf, ToolchainId) {
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let work = make_crate(tmp, "pub fn f() {}\n");
    let tc = local_toolchain(&work);

    let peer_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer_listener.local_addr().unwrap();
    let node_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    std::fs::write(
        home.join("config.toml"),
        format!(
            "node_name = \"sub\"\nlisten = \"127.0.0.1:{node_port}\"\npeers = [\"{peer_addr}\"]\nmdns = false\nslots = 2\nlocal_slots = 2\nprefer_remote = true\nauto_install = false\n"
        ),
    )
    .unwrap();
    run_ok(
        {
            let mut c = crabfarm(&home);
            c.args(["farm", "create", "--name", "sub"]);
            c
        },
        "farm create",
    );
    let node = Proc(
        crabfarm(&home)
            .arg("node")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    while !home.join("ipc.port").exists() {
        assert!(Instant::now() < deadline, "the node did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let code = run_ok(
        {
            let mut c = crabfarm(&home);
            c.args(["farm", "invite", "--addr"])
                .arg(format!("127.0.0.1:{node_port}"));
            c
        },
        "farm invite",
    );
    let peer_home = tmp.join("peer-home");
    std::fs::create_dir_all(&peer_home).unwrap();
    let (id, _) = farm::join(&peer_home, "fake", &JoinCode::parse(&code).unwrap())
        .await
        .expect("join the farm");

    let tls = Arc::new(TlsContext::new(&id).unwrap());
    let id = Arc::new(id);
    let job_seen = Arc::new(AtomicBool::new(false));
    let seen = job_seen.clone();
    let status = Status {
        node_id: id.node_id,
        node_name: "fake".into(),
        host_triple: tc.host.clone(),
        slots_total: 4,
        slots_busy: 0,
        queue_len: 0,
        speed: 100,
        toolchains: vec![tc.clone()],
        targets: vec![vec![tc.host.clone()]],
        accepting: true,
    };
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = peer_listener.accept().await else {
                return;
            };
            let (tls, id, status, seen) = (tls.clone(), id.clone(), status.clone(), seen.clone());
            tokio::spawn(async move {
                let Ok(Accepted::Peer { mut conn, .. }) = transport::accept(&tls, &id, tcp).await
                else {
                    return;
                };
                let mut tick = tokio::time::interval(Duration::from_secs(5));
                loop {
                    tokio::select! {
                        m = conn.recv() => match m {
                            Ok(Some(Message::PeerList { .. } | Message::StatusUpdate(_))) => {
                                let _ = conn.send(&Message::PeerList { peers: Vec::new() }).await;
                                let _ = conn.send(&Message::StatusUpdate(status.clone())).await;
                            }
                            Ok(Some(Message::Reserve { job_id })) => {
                                let _ = conn.send(&Message::Reserved { job_id }).await;
                                let Ok(Some(Message::JobRequest { .. })) = conn.recv().await else {
                                    return;
                                };
                                seen.store(true, Ordering::SeqCst);
                                match behavior {
                                    Peer::SilentAfterStart => {
                                        let _ = conn.send(&Message::Started).await;
                                        // A frozen machine: the socket stays open, nothing
                                        // is sent, and whatever arrives is not answered.
                                        loop {
                                            if !matches!(conn.recv().await, Ok(Some(_))) {
                                                return;
                                            }
                                        }
                                    }
                                }
                            }
                            _ => return,
                        },
                        _ = tick.tick() => {
                            let _ = conn.send(&Message::StatusUpdate(status.clone())).await;
                        }
                    }
                }
            });
        }
    });
    (node, FakePeer { job_seen }, work, tc)
}

/// The peer takes the job and then goes silent. The wrapper must build the
/// crate itself well inside a minute; without a liveness check the submitter
/// waits out the job timeout (30 min) and the node keeps the wrapper waiting
/// with keepalives of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_that_goes_silent_mid_job_does_not_stall_the_build() {
    let tmp = tempfile::tempdir().unwrap();
    let (_node, peer, work, tc) = start_farm(tmp.path(), Peer::SilentAfterStart).await;

    // Wait until the node has the peer's status (it dials the static peer).
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let out = crabfarm(&tmp.path().join("home"))
            .arg("status")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        if String::from_utf8_lossy(&out.stdout).contains("1 of 1 peers online") {
            break;
        }
        assert!(Instant::now() < deadline, "the peer never connected");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let _ = tc;

    let home = tmp.path().join("home");
    let work2 = work.clone();
    let ran = tokio::task::spawn_blocking(move || run_wrapper(&home, &work2, &[], 90))
        .await
        .unwrap();
    assert!(
        peer.job_seen.load(Ordering::SeqCst),
        "the job never reached the fake peer; the test proves nothing\n{}",
        ran.stderr
    );
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert!(
        ran.elapsed < Duration::from_secs(60),
        "the build stalled for {:?} behind a silent peer",
        ran.elapsed
    );
    assert!(
        work.join("out").join("libx-abc.rlib").exists(),
        "the local rustc did not run"
    );
}
