//! CF-19: a wrapper must not hang on a node that stopped answering, and a node
//! that is merely busy must say so. Loopback only; the real binary and a fake
//! IPC server, or the real node and a hand-driven IPC client.

mod common;

use common::{Behavior, FakeNode, make_crate, run_wrapper};
use crabfarm::proto::{ClientPath, JobId, JobSpec, Message, ToolchainId};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The node accepts the `Submit` and then never says a word: the wrapper must
/// give up after its silence limit (30 s) and build with the local rustc.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_goes_silent_does_not_hang_the_build() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = make_crate(tmp.path(), "pub fn f() {}\n");
    let node = FakeNode::start(&home, vec![Behavior::Silent]).await;

    let ran = run_wrapper(&home, &work, &[], 100);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert_eq!(node.submitted(), 1, "the job never reached the fake node");
    assert!(
        ran.elapsed >= Duration::from_secs(29),
        "gave up after {:?}, before the silence limit",
        ran.elapsed
    );
    assert!(
        work.join("out").join("libx-abc.rlib").exists(),
        "the local rustc did not run"
    );
}

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

fn spec(dir: &Path) -> JobSpec {
    let p = ClientPath::from_local(dir, dir).expect("client path");
    JobSpec {
        job_id: JobId([7; 16]),
        toolchain: ToolchainId {
            release: "1.100.0".into(),
            commit_hash: "a".repeat(40),
            commit_date: "2026-11-09".into(),
            host: "x86_64-pc-windows-msvc".into(),
        },
        target: "x86_64-pc-windows-msvc".into(),
        args: Vec::new(),
        cwd: p.clone(),
        env: Vec::new(),
        extra_env: Vec::new(),
        build_env: Vec::new(),
        inputs: Vec::new(),
        dirs: Vec::new(),
        out_dir: p,
        crate_name: "x".into(),
        no_retain: false,
        needs_host: false,
    }
}

fn submit(dir: &Path) -> Message {
    let input = ClientPath::from_local(&dir.join("lib.rs"), dir).expect("client path");
    Message::Submit {
        spec: spec(dir),
        rustc: "rustc".into(),
        produces: Vec::new(),
        externs: Vec::new(),
        input,
        search_dirs: Vec::new(),
        link_libs: Vec::new(),
        force_local: false,
    }
}

/// The real node: one local slot is taken by a first wrapper that never
/// finishes; a second `Submit` then waits for the slot without any other
/// frame, and must get keepalives meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_node_sends_keepalives_while_a_job_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    std::fs::write(
        home.join("config.toml"),
        format!(
            "node_name = \"ka\"\nlisten = \"127.0.0.1:{port}\"\nmdns = false\nslots = 2\nlocal_slots = 1\nauto_install = false\n"
        ),
    )
    .unwrap();
    let st = crabfarm(&home)
        .args(["farm", "create", "--name", "ka"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(st.success());
    let _node = Proc(
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

    // Wrapper one takes the only local slot and sits on it.
    let mut first = crabfarm::ipc::connect(&home).await.unwrap();
    first.send(&submit(tmp.path())).await.unwrap();
    let got = crabfarm::ipc::recv_within(&mut first, Duration::from_secs(20))
        .await
        .expect("no answer to the first Submit");
    assert!(matches!(got, Message::RunLocal { .. }), "{got:?}");

    // Wrapper two has to wait; the node must keep talking.
    let mut second = crabfarm::ipc::connect(&home).await.unwrap();
    second.send(&submit(tmp.path())).await.unwrap();
    let started = Instant::now();
    let mut keepalives = 0;
    while keepalives < 2 {
        let m = crabfarm::ipc::recv_within(&mut second, Duration::from_secs(9))
            .await
            .unwrap_or_else(|| {
                panic!(
                    "the node was silent for 9 s with a job waiting ({keepalives} keepalives after {:?})",
                    started.elapsed()
                )
            });
        match m {
            Message::Keepalive => keepalives += 1,
            other => panic!("unexpected frame while waiting for a slot: {other:?}"),
        }
    }
    // Releasing the slot lets the second job through.
    first
        .send(&Message::LocalDone {
            code: 0,
            produces: Vec::new(),
        })
        .await
        .unwrap();
    loop {
        match crabfarm::ipc::recv_within(&mut second, Duration::from_secs(20)).await {
            Some(Message::Keepalive) => {}
            Some(Message::RunLocal { .. }) => break,
            other => panic!("expected RunLocal, got {other:?}"),
        }
    }
}
