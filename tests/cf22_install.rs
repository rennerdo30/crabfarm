//! CF-22: the setup commands against throwaway directories only. Nothing here
//! touches PATH, the registry, autostart, services or the real cargo config:
//! `CARGO_HOME` and `CRABFARM_HOME` always point into a temp dir.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CLI: &str = env!("CARGO_BIN_EXE_crabfarm");
const DAEMON: &str = env!("CARGO_BIN_EXE_crabfarmd");

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

fn cli(home: &Path) -> Command {
    let mut c = Command::new(CLI);
    c.env("CRABFARM_HOME", home)
        .env_remove("CRABFARM_LOG")
        .stdin(Stdio::null());
    c
}

fn make_home(tmp: &Path) -> PathBuf {
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "node_name = \"one\"\nlisten = \"127.0.0.1:{}\"\nmdns = false\nslots = 2\nlocal_slots = 2\nauto_install = false\n",
            free_port()
        ),
    )
    .unwrap();
    let st = cli(&home)
        .args(["farm", "create", "--name", "one"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(st.success());
    home
}

fn wait_for(what: &str, secs: u64, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn crabfarmd_runs_the_node_refuses_a_second_and_stops_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let home = make_home(tmp.path());
    let mut first = Proc(
        Command::new(DAEMON)
            .env("CRABFARM_HOME", &home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for("the daemon's IPC port", 60, || {
        home.join("ipc.port").exists()
    });
    let log = home.join("logs").join("node.log");
    assert!(log.exists(), "crabfarmd must log to node.log");

    // A second daemon on the same directory leaves and says why in the log.
    let second = Command::new(DAEMON)
        .env("CRABFARM_HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(second.success(), "a refused second node exits cleanly");
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("another crabfarm node is already running"),
        "{text}"
    );
    assert!(first.0.try_wait().unwrap().is_none(), "the first node died");

    // Clean stop over IPC: the node exits by itself, no kill.
    let out = cli(&home).args(["node", "--stop"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("stopped"));
    wait_for("the daemon to exit", 20, || {
        first.0.try_wait().unwrap().is_some()
    });
    assert!(!home.join("ipc.port").exists(), "IPC port left behind");

    // Nothing running any more.
    let out = cli(&home).args(["node", "--stop"]).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("No node is running"));
}

#[test]
fn enable_and_disable_global_round_trip_in_a_temp_cargo_home() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cargo_home = tmp.path().join("cargo-home");
    std::fs::create_dir_all(&cargo_home).unwrap();
    let original = "# mine\n[build]\nrustc-wrapper = \"sccache\"\n\n[net]\nretry = 2\n";
    let file = cargo_home.join("config.toml");
    std::fs::write(&file, original).unwrap();

    let out = cli(&home)
        .env("CARGO_HOME", &cargo_home)
        .args(["enable", "--global", "--jobs", "12"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        text.contains("CRABFARM_INNER_WRAPPER = \"sccache\""),
        "{text}"
    );
    assert!(text.contains("jobs = 12"), "{text}");
    assert!(text.contains("# mine"));

    let out = cli(&home)
        .env("CARGO_HOME", &cargo_home)
        .args(["disable", "--global"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
}

#[test]
fn enable_outside_a_cargo_project_is_refused_without_global() {
    let tmp = tempfile::tempdir().unwrap();
    let lone = tmp.path().join("lone");
    std::fs::create_dir_all(&lone).unwrap();
    let out = cli(&tmp.path().join("home"))
        .current_dir(&lone)
        .args(["enable"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not inside a Cargo project"), "{err}");
    assert!(!lone.join(".cargo").exists(), "nothing may be created");
}

/// A stand-in for sccache: records one line per call, then runs the compiler.
fn build_fake_inner(dir: &Path) -> PathBuf {
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

/// The point of `enable`: a plain `cargo build` (no `crabfarm cargo`) runs
/// through crabfarm, and the earlier wrapper still compiles locally, because
/// cargo hands `[env]` to the wrapper process.
#[test]
fn plain_cargo_build_in_an_enabled_project_chains_the_inner_wrapper() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    std::fs::create_dir_all(proj.join(".cargo")).unwrap();
    std::fs::write(
        proj.join("Cargo.toml"),
        "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(proj.join("src/main.rs"), "fn main() {}\n").unwrap();
    let inner = build_fake_inner(tmp.path());
    let log = tmp.path().join("inner.log");
    // TOML literal string: Windows paths hold backslashes.
    std::fs::write(
        proj.join(".cargo/config.toml"),
        format!("[build]\nrustc-wrapper = '{}'\n", inner.display()),
    )
    .unwrap();

    let out = cli(&home)
        .current_dir(&proj)
        .env("CARGO_HOME", tmp.path().join("cargo-home"))
        .args(["enable", "--jobs", "2"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");

    let out = Command::new("cargo")
        .current_dir(&proj)
        .args(["build", "--offline"])
        .env("CRABFARM_HOME", &home)
        .env("INNER_LOG", &log)
        .env("CARGO_TARGET_DIR", tmp.path().join("target"))
        .env_remove("RUSTC_WRAPPER")
        .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
        .env_remove("CRABFARM_INNER_WRAPPER")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = std::fs::read_to_string(&log).expect("the inner wrapper never ran");
    assert!(
        calls.contains("--crate-name p"),
        "the local compile did not go through the inner wrapper:\n{calls}"
    );
}
