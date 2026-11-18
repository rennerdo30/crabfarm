//! CF-27 F1 and F2 through the real `crabfarm enable` command, in temp
//! directories only: the workspace root is chosen, a wrapper from outside the
//! file is chained, and an environment wrapper is warned about.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// A workspace `ws` with the member `ws/crates/foo`; returns (root, member).
fn workspace(base: &Path) -> (PathBuf, PathBuf) {
    let root = base.join("ws");
    let member = root.join("crates/foo");
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/foo\"]\nresolver = \"2\"\n",
    );
    write(
        &member.join("Cargo.toml"),
        "[package]\nname = \"foo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(&member.join("src/lib.rs"), "");
    (root, member)
}

fn enable(cwd: &Path, base: &Path, env: &[(&str, &str)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_crabfarm"));
    c.arg("enable")
        .current_dir(cwd)
        .env("CRABFARM_HOME", base.join("home"))
        .env("CARGO_HOME", base.join("cargo-home"))
        .env_remove("RUSTC_WRAPPER")
        .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
        .stdin(Stdio::null());
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().expect("run crabfarm enable")
}

#[test]
fn enable_in_a_member_writes_the_workspace_root_config() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, member) = workspace(tmp.path());
    let out = enable(&member, tmp.path(), &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cfg = root.join(".cargo/config.toml");
    assert!(cfg.is_file(), "no config at the workspace root");
    assert!(
        !member.join(".cargo/config.toml").exists(),
        "a config the member's own cargo run would ignore was written"
    );
    // It says which directory it chose.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Project root"), "{stdout}");
}

#[test]
fn a_wrapper_from_a_parent_directory_is_chained_not_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, member) = workspace(tmp.path());
    // The wrapper lives ABOVE the workspace, so the workspace's own file has none.
    write(
        &tmp.path().join(".cargo/config.toml"),
        "[build]\nrustc-wrapper = \"sccache\"\n",
    );
    let out = enable(&member, tmp.path(), &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(root.join(".cargo/config.toml")).unwrap();
    let t: toml::Table = toml::from_str(&text).unwrap();
    assert_eq!(
        t["env"]["CRABFARM_INNER_WRAPPER"].as_str(),
        Some("sccache"),
        "{text}"
    );
    // And disable leaves no wrapper of the parent's behind in this file.
    let mut c = Command::new(env!("CARGO_BIN_EXE_crabfarm"));
    let st = c
        .arg("disable")
        .current_dir(&member)
        .env("CRABFARM_HOME", tmp.path().join("home"))
        .env("CARGO_HOME", tmp.path().join("cargo-home"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(st.status.success());
    let after = std::fs::read_to_string(root.join(".cargo/config.toml")).unwrap();
    assert!(
        !after.contains("sccache") && !after.contains("rustc-wrapper"),
        "{after}"
    );
}

#[test]
fn a_wrapper_in_the_environment_is_warned_about_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let (_root, member) = workspace(tmp.path());
    let out = enable(
        &member,
        tmp.path(),
        &[("RUSTC_WRAPPER", "secret-looking-value")],
    );
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("RUSTC_WRAPPER"), "{err}");
    assert!(
        err.contains("not used") || err.contains("NOT used"),
        "{err}"
    );
    assert!(
        !err.contains("secret-looking-value"),
        "the value must not be printed"
    );
}
