//! CF-18: secrets must not reach a peer. The real `crabfarm` binary runs in
//! wrapper mode against a fake node (loopback IPC); the tests look at what the
//! wrapper puts into the `JobSpec` and at what it does after a peer's result.

mod common;

use common::{Behavior, FakeNode, make_crate, run_wrapper};

const SECRET: &str = "crabfarm-test-secret-value";

fn dep_file() -> String {
    "x-abc.d".to_string()
}

/// A `.d` file as a peer could return it, listing a variable the crate never
/// reads.
fn peer_dep_info(var: &str) -> String {
    format!("out/libx-abc.rmeta: src/lib.rs\n\nsrc/lib.rs:\n# env-dep:{var}\n")
}

/// The first leak: every `CARGO_*` variable of the build used to be sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registry_tokens_never_appear_in_a_job_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = make_crate(tmp.path(), "pub fn f() {}\n");
    let node = FakeNode::start(&home, vec![Behavior::RunLocal]).await;

    let ran = run_wrapper(
        &home,
        &work,
        &[
            ("CARGO_REGISTRY_TOKEN", SECRET),
            ("CARGO_REGISTRIES_PRIVATE_TOKEN", SECRET),
            ("CARGO_PKG_NAME", "x"),
            ("CARGO_MANIFEST_DIR", work.to_str().unwrap()),
        ],
        120,
    );
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert_eq!(node.submitted(), 1, "the job never reached the node");
    let names = node.env_names(0);
    assert!(
        names.iter().any(|n| n == "CARGO_PKG_NAME"),
        "the allow-listed variables must still travel: {names:?}"
    );
    for n in &names {
        let up = n.to_ascii_uppercase();
        assert!(
            !up.contains("TOKEN") && up != "CARGO_REGISTRY_TOKEN",
            "a credential was put into the job: {n}"
        );
    }
    let spec = &node.submits.lock().unwrap()[0].spec;
    assert!(spec.extra_env.is_empty(), "{:?}", spec.extra_env);
}

/// The second leak: a peer returns `# env-dep:GITHUB_TOKEN` in its `.d`; the
/// wrapper used to remember the name and forward the token from then on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_written_dep_info_never_makes_the_wrapper_forward_a_variable() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = make_crate(tmp.path(), "pub fn f() {}\n");
    let node = FakeNode::start(
        &home,
        vec![
            Behavior::PeerSuccess {
                dep_file_name: dep_file(),
                dep_info: peer_dep_info("GITHUB_TOKEN"),
            },
            Behavior::RunLocal,
        ],
    )
    .await;
    let env = [("GITHUB_TOKEN", SECRET)];

    // Build 1: the peer's `.d` makes the wrapper distrust the result and
    // rerun locally.
    let ran = run_wrapper(&home, &work, &env, 120);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert_eq!(node.submitted(), 1);

    // Build 2 (same data directory): the token must not be in the job, and
    // the crate must not have become local-only because of the peer's claim.
    let ran = run_wrapper(&home, &work, &env, 120);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert_eq!(node.submitted(), 2);
    let names = node.env_names(1);
    assert!(
        !names.iter().any(|n| n.eq_ignore_ascii_case("GITHUB_TOKEN")),
        "GITHUB_TOKEN was forwarded after a peer's dep-info named it: {names:?}"
    );
    assert!(
        !node.submits.lock().unwrap()[1].force_local,
        "a peer's claim must not pin the crate to local builds"
    );
    assert!(node.submits.lock().unwrap()[1].spec.extra_env.is_empty());
}

/// A crate that really reads a variable the farm does not forward: the local
/// rerun's own `.d` shows it, the name is recorded, and from then on the crate
/// stays local (and the value is still never sent).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_variable_the_local_build_reads_keeps_the_crate_local_not_forwarded() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = make_crate(
        tmp.path(),
        "pub fn f() -> Option<&'static str> { option_env!(\"CF18_SECRET_INPUT\") }\n",
    );
    let node = FakeNode::start(
        &home,
        vec![
            Behavior::PeerSuccess {
                dep_file_name: dep_file(),
                dep_info: peer_dep_info("CF18_SECRET_INPUT"),
            },
            Behavior::RunLocal,
        ],
    )
    .await;
    let env = [("CF18_SECRET_INPUT", SECRET)];

    let ran = run_wrapper(&home, &work, &env, 120);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    let recorded = std::fs::read_to_string(home.join("env_forward.toml")).unwrap_or_default();
    assert!(
        recorded.contains("CF18_SECRET_INPUT"),
        "the local .d names the variable, so it must be recorded:\n{recorded}"
    );
    assert!(!recorded.contains(SECRET), "a value was written to disk");

    let ran = run_wrapper(&home, &work, &env, 120);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    let subs = node.submits.lock().unwrap();
    assert_eq!(subs.len(), 2);
    assert!(
        subs[1].force_local,
        "the crate must stay local while the variable is set"
    );
    assert!(
        subs[1].spec.env.is_empty(),
        "a local-only job carries no env"
    );
}

/// Listing the variable in `forward_env` is the one way to send it: the crate
/// then goes to the peer again and the name is part of `extra_env`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_in_forward_env_is_forwarded_and_does_not_block_the_crate() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "forward_env = [\"CF18_SECRET_INPUT\"]\n",
    )
    .unwrap();
    let work = make_crate(
        tmp.path(),
        "pub fn f() -> Option<&'static str> { option_env!(\"CF18_SECRET_INPUT\") }\n",
    );
    // A stale record from an earlier build must not matter once it is listed.
    std::fs::write(
        home.join("env_forward.toml"),
        "x = [\"CF18_SECRET_INPUT\"]\n",
    )
    .unwrap();
    let node = FakeNode::start(&home, vec![Behavior::RunLocal]).await;
    let ran = run_wrapper(&home, &work, &[("CF18_SECRET_INPUT", SECRET)], 120);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    let subs = node.submits.lock().unwrap();
    assert!(!subs[0].force_local);
    assert!(
        subs[0]
            .spec
            .extra_env
            .iter()
            .any(|n| n == "CF18_SECRET_INPUT"),
        "{:?}",
        subs[0].spec.extra_env
    );
}
