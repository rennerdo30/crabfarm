//! CF-21: a remote compile error is never authoritative (registry crates
//! included), and the names a build script emits with `rustc-env` travel with
//! the job. The real `crabfarm` binary runs in wrapper mode against a fake node.

mod common;

use common::{Behavior, FakeNode, make_crate, run_wrapper};

const REMOTE_ONLY_ERROR: &str = "error: remote-only failure of the peer";

/// A crate whose source lies under a fake `CARGO_HOME` registry directory: the
/// peer's error must not end the build, the local rerun judges it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_failure_of_a_registry_crate_reruns_locally() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cargo_home = tmp.path().join("cargo_home");
    let registry = cargo_home.join("registry").join("src").join("index-x");
    std::fs::create_dir_all(&registry).unwrap();
    let work = make_crate(&registry, "pub fn f() {}\n");
    let node = FakeNode::start(
        &home,
        vec![Behavior::PeerCompileError {
            stderr_line: REMOTE_ONLY_ERROR.to_string(),
        }],
    )
    .await;

    let ran = run_wrapper(
        &home,
        &work,
        &[("CARGO_HOME", cargo_home.to_str().unwrap())],
        120,
    );
    assert_eq!(
        ran.code,
        Some(0),
        "the local rerun compiles the crate, so the build succeeds\nstderr:\n{}",
        ran.stderr
    );
    assert_eq!(node.submitted(), 1);
    assert_eq!(
        node.retries.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the wrapper must ask for a local rerun"
    );
    assert!(
        !ran.stderr.contains(REMOTE_ONLY_ERROR) && !ran.stdout.contains(REMOTE_ONLY_ERROR),
        "the peer's diagnostics must not reach the user:\n{}",
        ran.stderr
    );
    assert!(
        work.join("out").join("libx-abc.rlib").exists(),
        "the local rerun must have produced the library"
    );
}

/// A build script's `rustc-env` names are found in the output cargo wrote and
/// declared in `build_env`; names that only appear in the file but are not set
/// in the wrapper's environment are skipped, and the never-list holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_script_rustc_env_names_travel_in_build_env() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = make_crate(tmp.path(), "pub const B: &str = env!(\"CF_BUILT\");\n");
    // `<build>/<pkg>/<hash>/{out, run/stdout}`, the layout of current cargo.
    let unit = tmp.path().join("build").join("x").join("0123456789abcdef");
    std::fs::create_dir_all(unit.join("out")).unwrap();
    std::fs::create_dir_all(unit.join("run")).unwrap();
    std::fs::write(
        unit.join("run").join("stdout"),
        "cargo:rerun-if-changed=build.rs\ncargo::rustc-env=CF_BUILT=yes\ncargo:rustc-env=CF_UNSET=1\ncargo:rustc-env=PATH=/evil\n",
    )
    .unwrap();
    let node = FakeNode::start(&home, vec![Behavior::RunLocal]).await;
    let out_dir = unit.join("out");

    let ran = run_wrapper(
        &home,
        &work,
        &[
            ("OUT_DIR", out_dir.to_str().unwrap()),
            ("CF_BUILT", "yes"),
            ("PATH_LIKE_UNLISTED", "no"),
        ],
        120,
    );
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    let subs = node.submits.lock().unwrap();
    let spec = &subs[0].spec;
    assert_eq!(spec.build_env, vec!["CF_BUILT".to_string()]);
    assert!(
        spec.env.iter().any(|(k, _)| k == "CF_BUILT"),
        "the value travels as cargo set it"
    );
    assert!(
        !spec.env.iter().any(|(k, _)| k == "PATH_LIKE_UNLISTED"),
        "only declared names go along"
    );
    assert!(spec.extra_env.is_empty(), "{:?}", spec.extra_env);
}

/// No build-script output to be found while `OUT_DIR` is set: nothing is
/// guessed, nothing is declared, the job still goes out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_build_script_output_declares_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = make_crate(tmp.path(), "pub fn f() {}\n");
    let out_dir = tmp.path().join("build").join("x").join("hash").join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let node = FakeNode::start(&home, vec![Behavior::RunLocal]).await;
    let ran = run_wrapper(
        &home,
        &work,
        &[("OUT_DIR", out_dir.to_str().unwrap()), ("CF_BUILT", "yes")],
        120,
    );
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    let subs = node.submits.lock().unwrap();
    assert!(subs[0].spec.build_env.is_empty());
    assert!(!subs[0].spec.env.iter().any(|(k, _)| k == "CF_BUILT"));
}
