//! CF-25: a peer that goes silent in the middle of a job (lid closed, Wi-Fi
//! gone, no reset) must not stall the build. The real node and the real
//! wrapper run as child processes; the peer is a fake farm member in this
//! process that accepts the job and then says nothing. Loopback only, mDNS off.

mod common;

use common::{Behavior, FakeNode, make_crate, run_wrapper};
use crabfarm::farm::{self, JoinCode};
use crabfarm::proto::{Message, Status, ToolchainId};
use crabfarm::transport::{self, Accepted, TlsContext};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Notify;

/// These tests start whole farms and wait on timing; run side by side on a
/// busy machine they starve each other (CF-27: the "wrapper has gone" check
/// failed when the node noticed the closed connection late). One at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
#[derive(Clone)]
enum Peer {
    /// Take the `JobRequest`, say `Started`, then never send another byte and
    /// keep the connection open.
    SilentAfterStart,
    /// Take the `JobRequest`, wait for `go`, then answer as a peer that built
    /// the crate: `Output` (one file in the job's out-dir), its chunks, `Exit 0`.
    AnswerOnSignal { go: Arc<Notify>, data: Vec<u8> },
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
            let behavior = behavior.clone();
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
                                let Ok(Some(Message::JobRequest { spec, .. })) = conn.recv().await
                                else {
                                    return;
                                };
                                seen.store(true, Ordering::SeqCst);
                                match behavior {
                                    Peer::AnswerOnSignal { go, data } => {
                                        go.notified().await;
                                        let _ = conn.send(&Message::Started).await;
                                        let out = spec.out_dir.to_local();
                                        let Some(path) = crabfarm::proto::ClientPath::from_local(
                                            &out.join("libx-abc.rlib"),
                                            &out,
                                        ) else {
                                            return;
                                        };
                                        let hash = crabfarm::proto::BlobHash::of(&data);
                                        let _ = conn
                                            .send(&Message::Output {
                                                path,
                                                hash,
                                                size: data.len() as u64,
                                            })
                                            .await;
                                        for (offset, last, chunk) in crabfarm::cas::chunks(&data) {
                                            let _ = conn
                                                .send(&Message::BlobChunk {
                                                    hash,
                                                    offset,
                                                    last,
                                                    data: chunk,
                                                })
                                                .await;
                                        }
                                        let _ = conn.send(&Message::Exit { code: 0 }).await;
                                        // Keep the connection until the node closes it.
                                        while let Ok(Some(_)) = conn.recv().await {}
                                        return;
                                    }
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

/// Wait until the node has the peer's status (it dials the static peer).
async fn wait_for_peer(home: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let out = crabfarm(home)
            .arg("status")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        if String::from_utf8_lossy(&out.stdout).contains("1 of 1 peers online") {
            return;
        }
        assert!(Instant::now() < deadline, "the peer never connected");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// A `Submit` for the crate in `work`, as a wrapper would send it.
fn submit_for(work: &Path, tc: &ToolchainId) -> Message {
    use crabfarm::proto::{ClientPath, JobId, JobSpec};
    let out = work.join("out");
    let out_dir = ClientPath::from_local(&out, &out).expect("client path");
    let input = ClientPath::from_local(&work.join("src").join("lib.rs"), work).expect("path");
    Message::Submit {
        spec: JobSpec {
            job_id: JobId([9; 16]),
            toolchain: tc.clone(),
            target: tc.host.clone(),
            args: Vec::new(),
            cwd: ClientPath::from_local(work, work).expect("client path"),
            env: Vec::new(),
            extra_env: Vec::new(),
            build_env: Vec::new(),
            inputs: Vec::new(),
            dirs: Vec::new(),
            out_dir,
            crate_name: "x".into(),
            no_retain: false,
            needs_host: false,
        },
        rustc: "rustc".into(),
        produces: Vec::new(),
        externs: Vec::new(),
        input,
        search_dirs: Vec::new(),
        link_libs: Vec::new(),
        force_local: false,
    }
}

/// Send a job to the fake peer as a wrapper would, let the peer answer, and
/// say whether its output file appeared. `keep_wrapper`: the wrapper's IPC
/// connection stays open (false: it is closed before the peer answers, as a
/// wrapper does that gave up on a node that looked silent).
async fn answer_after_wrapper(keep_wrapper: bool) -> (PathBuf, bool) {
    let tmp = tempfile::tempdir().unwrap();
    let go = Arc::new(Notify::new());
    let (_node, peer, work, tc) = start_farm(
        tmp.path(),
        Peer::AnswerOnSignal {
            go: go.clone(),
            data: b"remote rlib".to_vec(),
        },
    )
    .await;
    let home = tmp.path().join("home");
    wait_for_peer(&home).await;

    let mut conn = crabfarm::ipc::connect(&home).await.expect("ipc");
    conn.send(&submit_for(&work, &tc)).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !peer.job_seen.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the job never reached the peer");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let target = work.join("out").join("libx-abc.rlib");
    if keep_wrapper {
        go.notify_one();
        // The relayed `Exit` shows the node is done with the job.
        loop {
            match crabfarm::ipc::recv_within(&mut conn, Duration::from_secs(20)).await {
                Some(Message::Exit { code: 0 }) => break,
                Some(Message::Keepalive) => {}
                other => panic!("expected the peer's Exit, got {other:?}"),
            }
        }
    } else {
        drop(conn);
        // Time for the node to notice the closed connection, even when the
        // machine is busy, before the peer answers.
        tokio::time::sleep(Duration::from_secs(3)).await;
        go.notify_one();
        tokio::time::sleep(Duration::from_secs(6)).await;
    }
    let exists = target.exists();
    (target, exists)
}

/// Control: with the wrapper still waiting the peer's output lands in the
/// client's out-dir, so the next test can fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_output_of_a_remote_job_is_written_while_the_wrapper_waits() {
    let _serial = SERIAL.lock().await;
    let (target, exists) = answer_after_wrapper(true).await;
    assert!(exists, "{} was not written", target.display());
}

/// CF-25: a node that was only stalled resumes after the wrapper went local:
/// it must not write the remote output over the local rustc's files.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_output_is_not_written_once_the_wrapper_has_gone() {
    let _serial = SERIAL.lock().await;
    let (target, exists) = answer_after_wrapper(false).await;
    assert!(
        !exists,
        "{} was written by a node whose wrapper had gone local",
        target.display()
    );
}

/// The peer takes the job and then goes silent. The wrapper must build the
/// crate itself well inside a minute; without a liveness check the submitter
/// waits out the job timeout (30 min) and the node keeps the wrapper waiting
/// with keepalives of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_that_goes_silent_mid_job_does_not_stall_the_build() {
    let _serial = SERIAL.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let (_node, peer, work, _tc) = start_farm(tmp.path(), Peer::SilentAfterStart).await;
    wait_for_peer(&tmp.path().join("home")).await;

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

/// CF-25: the wrapper that gave up on a silent node closes its IPC connection
/// BEFORE the local rustc starts. A node that only stalled and resumes then
/// sees the closed connection instead of a wrapper it may still write for.
/// (On the old code the connection stayed open until rustc had finished.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wrapper_closes_its_ipc_connection_before_the_local_rustc_runs() {
    let _serial = SERIAL.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = make_crate(
        tmp.path(),
        "pub fn f() {}
",
    );
    let rlib = work.join("out").join("libx-abc.rlib");
    let node = FakeNode::start(
        &home,
        vec![Behavior::SilentWatching {
            watch: rlib.clone(),
        }],
    )
    .await;

    let ran = tokio::task::spawn_blocking({
        let (home, work) = (home.clone(), work.clone());
        move || run_wrapper(&home, &work, &[], 90)
    })
    .await
    .unwrap();
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert!(rlib.exists(), "the local rustc did not run");
    let deadline = Instant::now() + Duration::from_secs(5);
    while node.closed_with_file.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "the node never saw the connection close"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        *node.closed_with_file.lock().unwrap(),
        vec![false],
        "the connection was still open when the local rustc had written its output"
    );
}

/// CF-25: after one wrapper found the node silent, the wrappers that follow do
/// not wait 30 s each: they see the short-lived marker and build locally at
/// once, without talking to the node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_just_went_silent_is_skipped_by_the_next_wrapper() {
    let _serial = SERIAL.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = make_crate(tmp.path(), "pub fn f() {}\n");
    let node = FakeNode::start(&home, vec![Behavior::Silent]).await;

    let first = tokio::task::spawn_blocking({
        let (home, work) = (home.clone(), work.clone());
        move || run_wrapper(&home, &work, &[], 90)
    })
    .await
    .unwrap();
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    assert!(
        first.elapsed >= Duration::from_secs(29),
        "{:?}",
        first.elapsed
    );
    assert!(
        home.join("node-unresponsive").exists(),
        "the silent node was not recorded"
    );
    assert_eq!(node.submitted(), 1);

    // The local rustc of the first run wrote this; the second must write it again.
    std::fs::remove_file(work.join("out").join("libx-abc.rlib")).unwrap();
    let second = tokio::task::spawn_blocking({
        let (home, work) = (home.clone(), work.clone());
        move || run_wrapper(&home, &work, &[], 20)
    })
    .await
    .unwrap();
    assert_eq!(second.code, Some(0), "{}", second.stderr);
    assert!(
        second.elapsed < Duration::from_secs(15),
        "the second wrapper waited {:?} for a node known to be silent",
        second.elapsed
    );
    assert_eq!(
        node.submitted(),
        1,
        "the second wrapper asked the node anyway"
    );
    assert!(work.join("out").join("libx-abc.rlib").exists());
}

/// CF-25: a running node that answers removes the marker by itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_answers_removes_the_unresponsive_marker() {
    let _serial = SERIAL.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let (_node, _peer, _work, _tc) = start_farm(tmp.path(), Peer::SilentAfterStart).await;
    let home = tmp.path().join("home");
    crabfarm::ipc::mark_unresponsive(&home);
    assert!(home.join("node-unresponsive").exists());
    let deadline = Instant::now() + Duration::from_secs(20);
    while home.join("node-unresponsive").exists() {
        assert!(
            Instant::now() < deadline,
            "the node left the marker in place"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
