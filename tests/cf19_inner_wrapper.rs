//! CF-19: a rustc wrapper that was configured before crabfarm (sccache, say)
//! keeps running the LOCAL compiles; crabfarm never chains to itself.

mod common;

use common::{Behavior, FakeNode, make_crate, run_wrapper};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A stand-in for sccache: records its arguments (one line per call) in the
/// file named by `INNER_LOG`, then runs the compiler it was given.
fn build_fake_wrapper(dir: &Path) -> PathBuf {
    let src = dir.join("inner.rs");
    std::fs::write(
        &src,
        r#"
use std::io::Write;
use std::process::Command;
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let mut f = std::fs::OpenOptions::new().create(true).append(true)
        .open(std::env::var("INNER_LOG").unwrap()).unwrap();
    writeln!(f, "{}", a.join(" ")).unwrap();
    let st = Command::new(&a[0]).args(&a[1..]).status().unwrap();
    std::process::exit(st.code().unwrap_or(1));
}
"#,
    )
    .unwrap();
    let exe = dir.join(if cfg!(windows) { "inner.exe" } else { "inner" });
    let st = Command::new("rustc")
        .arg(&src)
        .arg("-o")
        .arg(&exe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("rustc");
    assert!(st.success(), "could not build the stand-in wrapper");
    exe
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_runs_go_through_the_inner_wrapper() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = make_crate(tmp.path(), "pub fn f() {}\n");
    let inner = build_fake_wrapper(tmp.path());
    let log = tmp.path().join("inner.log");
    let env = [
        ("CRABFARM_INNER_WRAPPER", inner.to_str().unwrap()),
        ("INNER_LOG", log.to_str().unwrap()),
    ];

    // The node tells the wrapper to run locally: that run is the inner one's.
    let node = FakeNode::start(&home, vec![Behavior::RunLocal]).await;
    let ran = run_wrapper(&home, &work, &env, 120);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert_eq!(node.submitted(), 1);
    let calls = std::fs::read_to_string(&log).expect("the inner wrapper never ran");
    assert!(
        calls.lines().count() == 1
            && calls.starts_with("rustc ")
            && calls.contains("--crate-name x"),
        "unexpected inner calls:\n{calls}"
    );
    assert!(work.join("out").join("libx-abc.rlib").exists());
}

/// A job a peer built is not compiled here at all, so the inner wrapper (which
/// caches and runs compilers) is not involved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_result_does_not_involve_the_inner_wrapper() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = make_crate(
        tmp.path(),
        "pub fn f() {}
",
    );
    let inner = build_fake_wrapper(tmp.path());
    let log = tmp.path().join("inner.log");
    let node = FakeNode::start(
        &home,
        vec![Behavior::PeerSuccess {
            dep_file_name: "x-abc.d".into(),
            dep_info: "out/libx-abc.rmeta: src/lib.rs
"
            .into(),
        }],
    )
    .await;
    let ran = run_wrapper(
        &home,
        &work,
        &[
            ("CRABFARM_INNER_WRAPPER", inner.to_str().unwrap()),
            ("INNER_LOG", log.to_str().unwrap()),
        ],
        120,
    );
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert_eq!(node.submitted(), 1);
    assert!(!log.exists(), "the inner wrapper ran for a remote result");
}

#[test]
fn without_a_node_the_inner_wrapper_still_runs_the_compile() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let work = make_crate(tmp.path(), "pub fn f() {}\n");
    let inner = build_fake_wrapper(tmp.path());
    let log = tmp.path().join("inner.log");
    let ran = run_wrapper(
        &home,
        &work,
        &[
            ("CRABFARM_INNER_WRAPPER", inner.to_str().unwrap()),
            ("INNER_LOG", log.to_str().unwrap()),
        ],
        120,
    );
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    let calls = std::fs::read_to_string(&log).expect("the inner wrapper never ran");
    assert!(calls.contains("--crate-name x"), "{calls}");
}

/// Chaining crabfarm to itself would loop forever.
#[test]
fn crabfarm_is_never_chained_to_itself() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let work = make_crate(tmp.path(), "pub fn f() {}\n");
    let ran = run_wrapper(&home, &work, &[("CRABFARM_INNER_WRAPPER", common::BIN)], 60);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert!(work.join("out").join("libx-abc.rlib").exists());
}
