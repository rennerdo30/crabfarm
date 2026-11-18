//! CF-27: `cargo crabfarm <args>` (cargo runs `cargo-crabfarm crabfarm <args>`)
//! must behave exactly like `crabfarm cargo <args>`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A stand-in for cargo: writes its arguments and the file name of
/// RUSTC_WRAPPER to the file named by `FAKE_CARGO_LOG`.
fn build_fake_cargo(dir: &Path) -> PathBuf {
    let src = dir.join("cargo_fake.rs");
    std::fs::write(
        &src,
        r#"
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let w = std::env::var("RUSTC_WRAPPER").unwrap_or_default();
    let stem = std::path::Path::new(&w).file_stem()
        .map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    std::fs::write(std::env::var("FAKE_CARGO_LOG").unwrap(),
        format!("{}\nwrapper={}\n", a.join(" "), stem)).unwrap();
}
"#,
    )
    .unwrap();
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
    let st = Command::new("rustc")
        .arg(&src)
        .arg("-o")
        .arg(&exe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("rustc");
    assert!(st.success(), "could not build the stand-in cargo");
    bin
}

fn run(exe: &str, args: &[&str], bin: &Path, home: &Path, log: &Path) -> (Option<i32>, String) {
    let _ = std::fs::remove_file(log);
    let out = Command::new(exe)
        .args(args)
        .env("CRABFARM_HOME", home)
        .env("PATH", bin)
        .env("FAKE_CARGO_LOG", log)
        .env_remove("RUSTC_WRAPPER")
        .env_remove("CRABFARM_INNER_WRAPPER")
        .env_remove("CARGO_BUILD_JOBS")
        .stdin(Stdio::null())
        .output()
        .expect("run");
    (
        out.status.code(),
        std::fs::read_to_string(log).unwrap_or_default(),
    )
}

#[test]
fn cargo_crabfarm_passes_the_same_arguments_and_wrapper_as_crabfarm_cargo() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = build_fake_cargo(tmp.path());
    let home = tmp.path().join("home");
    let log = tmp.path().join("log.txt");
    for args in [
        &["build", "--release", "-p", "x"][..],
        &["build", "--help"],
        &["test", "--", "--nocapture"],
        &["--version"],
    ] {
        let mut via_cargo = vec!["crabfarm"];
        via_cargo.extend_from_slice(args);
        let (code_a, seen_a) = run(
            env!("CARGO_BIN_EXE_cargo-crabfarm"),
            &via_cargo,
            &bin,
            &home,
            &log,
        );
        let mut direct = vec!["cargo"];
        direct.extend_from_slice(args);
        let (code_b, seen_b) = run(env!("CARGO_BIN_EXE_crabfarm"), &direct, &bin, &home, &log);
        assert_eq!(code_a, Some(0), "{args:?}");
        assert_eq!(code_a, code_b, "{args:?}");
        assert!(seen_a.contains("wrapper=crabfarm"), "{args:?}: {seen_a}");
        assert_eq!(seen_a, seen_b, "{args:?}");
        assert!(seen_a.starts_with(&args.join(" ")), "{seen_a}");
    }
}
