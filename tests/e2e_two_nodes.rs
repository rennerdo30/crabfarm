//! End to end: two real nodes (child processes of the built binary, loopback
//! only, mDNS off) build `tests/fixture` through `crabfarm cargo`.
//!
//! Safety (owner PC): everything binds 127.0.0.1 only, so no firewall dialog
//! can appear; every child has a timeout; only the PIDs started here are killed.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_crabfarm");

/// A started child that is killed (by its own handle) when dropped.
struct Proc {
    child: Child,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Output {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn crabfarm(home: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.env("CRABFARM_HOME", home);
    // Whatever the surrounding `cargo test` run set must not leak into the builds under test.
    for v in ["RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_TARGET_DIR", "CRABFARM_LOG"] {
        c.env_remove(v);
    }
    c
}

/// Run to completion within `secs`, or kill exactly this child and fail.
fn run(mut cmd: Command, secs: u64) -> Output {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn");
    // Drain the pipes on threads so a chatty child cannot block on a full pipe.
    let mut out = child.stdout.take().expect("stdout");
    let mut err = child.stderr.take().expect("stderr");
    let t_out = std::thread::spawn(move || {
        let mut s = Vec::new();
        std::io::Read::read_to_end(&mut out, &mut s).ok();
        s
    });
    let t_err = std::thread::spawn(move || {
        let mut s = Vec::new();
        std::io::Read::read_to_end(&mut err, &mut s).ok();
        s
    });
    let deadline = Instant::now() + Duration::from_secs(secs);
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = String::from_utf8_lossy(&t_out.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&t_err.join().unwrap_or_default()).into_owned();
    assert!(status.is_some(), "command timed out after {secs}s\nstdout:\n{stdout}\nstderr:\n{stderr}");
    Output { ok: status.is_some_and(|s| s.success()), stdout, stderr }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

fn write_config(home: &Path, name: &str, port: u16, local_slots: u32, prefer_remote: bool) {
    std::fs::create_dir_all(home).expect("home");
    let text = format!(
        "node_name = \"{name}\"\nlisten = \"127.0.0.1:{port}\"\nmdns = false\nslots = 4\nlocal_slots = {local_slots}\nprefer_remote = {prefer_remote}\nauto_install = false\n"
    );
    std::fs::write(home.join("config.toml"), text).expect("config");
}

fn start_node(home: &Path) -> Proc {
    let mut c = crabfarm(home);
    c.arg("node").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let child = c.spawn().expect("spawn node");
    let p = Proc { child };
    let deadline = Instant::now() + Duration::from_secs(60);
    while !home.join("ipc.port").exists() {
        assert!(Instant::now() < deadline, "node did not start; log:\n{}", node_log(home));
        std::thread::sleep(Duration::from_millis(100));
    }
    p
}

fn node_log(home: &Path) -> String {
    let text = std::fs::read_to_string(home.join("logs").join("node.log")).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(40)..].join("\n")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for e in std::fs::read_dir(from).expect("read_dir") {
        let e = e.expect("entry");
        let dest = to.join(e.file_name());
        if e.file_type().expect("type").is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), &dest).expect("copy");
        }
    }
}

fn counter(status: &str, label: &str) -> u64 {
    // "jobs: 3 remote ok, 0 remote failed, ..."
    status
        .lines()
        .flat_map(|l| l.split(','))
        .find_map(|part| {
            let part = part.trim().trim_start_matches("jobs:").trim();
            part.strip_suffix(label).and_then(|n| n.trim().parse().ok())
        })
        .unwrap_or_else(|| panic!("no `{label}` in status:\n{status}"))
}

fn cargo_build(home: &Path, work: &Path, target: &Path) -> Output {
    let mut c = crabfarm(home);
    c.args(["cargo", "build", "--release", "--color", "never", "--manifest-path"])
        .arg(work.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", target)
        // Nothing may be fetched.
        .env("CARGO_NET_OFFLINE", "true");
    run(c, 150)
}

fn app_binary(target: &Path) -> PathBuf {
    target.join("release").join(if cfg!(windows) { "app.exe" } else { "app" })
}

#[test]
fn two_nodes_build_the_fixture_remotely() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home1 = tmp.path().join("home1");
    let home2 = tmp.path().join("home2");
    let work = tmp.path().join("work");
    let target = tmp.path().join("target");
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixture"), &work);

    let (p1, p2) = (free_port(), free_port());
    write_config(&home1, "node-one", p1, 1, true);
    write_config(&home2, "node-two", p2, 2, false);

    // Farm on node one, node two joins with an invite.
    let mut c = crabfarm(&home1);
    c.args(["farm", "create", "--name", "node-one"]);
    assert!(run(c, 60).ok, "farm create failed");
    let _node1 = start_node(&home1);

    let mut c = crabfarm(&home1);
    c.args(["farm", "invite", "--addr"]).arg(format!("127.0.0.1:{p1}"));
    let invite = run(c, 30);
    assert!(invite.ok, "farm invite failed: {}", invite.stderr);
    let code = invite.stdout.trim().to_string();
    assert!(code.starts_with("cf1-"), "unexpected join code shape");

    let mut c = crabfarm(&home2);
    c.args(["farm", "join", &code, "--name", "node-two"]);
    let join = run(c, 60);
    assert!(join.ok, "farm join failed: {}\nnode one log:\n{}", join.stderr, node_log(&home1));
    let _node2 = start_node(&home2);

    // Wait until node one has node two's status (toolchains included).
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let mut c = crabfarm(&home1);
        c.arg("status");
        let st = run(c, 20);
        if st.stdout.contains("1 of 1 peers online") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "peers never connected:\n{}\nnode one log:\n{}\nnode two log:\n{}",
            st.stdout,
            node_log(&home1),
            node_log(&home2)
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    // 1. A clean build: proc macro, build script, warning, binary.
    let b1 = cargo_build(&home1, &work, &target);
    let dump = || format!("stdout:\n{}\nstderr:\n{}\nnode one log:\n{}\nnode two log:\n{}", b1.stdout, b1.stderr, node_log(&home1), node_log(&home2));
    assert!(b1.ok, "build failed\n{}", dump());
    let run_app = run(Command::new(app_binary(&target)), 30);
    assert!(run_app.ok, "app failed to run");
    let mut lines = run_app.stdout.lines();
    assert_eq!(lines.next(), Some("answer=42 generated=from-build-script"), "proc macro or OUT_DIR include broke");
    // CF-10: the `embed` crate bakes `env!("CARGO_MANIFEST_DIR")` into its rlib. Built on the
    // peer that is the peer's sandbox; crabfarm must notice and rebuild it locally.
    let client_embed = work.join("embed").to_string_lossy().into_owned();
    assert_eq!(
        lines.next(),
        Some(format!("embedded-manifest-dir={client_embed}").as_str()),
        "the app embeds a path that is not the client's manifest dir\n{}",
        dump()
    );

    let mut c = crabfarm(&home1);
    c.arg("status");
    let st = run(c, 20);
    assert!(st.stdout.contains("SandboxPathInOutput 1"), "exactly one job (embed) should be rerun for its sandbox path:\n{}\n{}", st.stdout, dump());
    let remote_ok = counter(&st.stdout, "remote ok");
    assert!(remote_ok >= 3, "expected derive_mac, util and core_lib to run remotely, got {remote_ok}\n{}\n{}", st.stdout, dump());

    // The warning names the client's file, never a sandbox of the peer.
    let rel = format!("core_lib{}src{}lib.rs", std::path::MAIN_SEPARATOR, std::path::MAIN_SEPARATOR);
    assert!(b1.stderr.contains("deliberately_unused"), "core_lib warning missing\n{}", dump());
    assert!(b1.stderr.contains(&rel), "warning does not show the client path {rel}\n{}", dump());
    let peer_dir = home2.to_string_lossy().replace('/', "\\");
    assert!(!b1.stderr.contains(&peer_dir) && !b1.stderr.contains("jobs"), "a sandbox path leaked into stderr\n{}", dump());
    // A warning inside the build script's OUT_DIR is an absolute path in the
    // job; it must come back as the client's target directory.
    assert!(b1.stderr.contains("generated_but_unused"), "util warning missing\n{}", dump());
    let client_target = target.to_string_lossy().into_owned();
    assert!(b1.stderr.contains(&client_target), "OUT_DIR warning does not show the client target dir {client_target}\n{}", dump());

    // 2. Same build again: dep-info points at client paths, so cargo sees everything fresh.
    let b2 = cargo_build(&home1, &work, &target);
    assert!(b2.ok, "second build failed\n{}", b2.stderr);
    assert!(!b2.stderr.contains("Compiling"), "second build recompiled, dep-info is not rewritten:\n{}", b2.stderr);

    // 3. A compile error is reported with client paths and a nonzero exit.
    let lib = work.join("core_lib").join("src").join("lib.rs");
    std::fs::write(&lib, "pub fn greeting() -> String { let x: u32 = \"not a number\"; String::new() }\n").expect("break core_lib");
    let b3 = cargo_build(&home1, &work, &target);
    assert!(!b3.ok, "a broken crate must fail the build");
    assert!(b3.stderr.contains("error"), "no error in stderr:\n{}", b3.stderr);
    assert!(b3.stderr.contains(&rel), "error does not show the client path {rel}:\n{}", b3.stderr);
    assert!(!b3.stderr.contains(&peer_dir), "a sandbox path leaked into the error:\n{}", b3.stderr);
    let mut c = crabfarm(&home1);
    c.arg("status");
    let st = run(c, 20);
    assert!(st.stdout.contains("RemoteCompileErrorRetry"), "the failing crate should have been judged by a local rerun:\n{}", st.stdout);
}
