//! `crabfarm cargo <args>`: cargo with crabfarm as `RUSTC_WRAPPER` and a
//! farm-sized `-j` (DESIGN sections 2 and 4).

use crate::config::{self, Config};
use crate::ipc;
use crate::proto::Message;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Environment variable through which `crabfarm cargo` hands the wrapper that
/// was configured before it (sccache, say) to the wrapper process: local rustc
/// runs go through it, remote runs are unaffected (CF-19).
pub const INNER_WRAPPER_ENV: &str = "CRABFARM_INNER_WRAPPER";

/// `-j` cap (DESIGN section 4).
pub const MAX_JOBS: u32 = 128;

/// Subcommands that accept `-j`; anything else gets no extra flag.
const JOBS_COMMANDS: [&str; 19] = [
    "build", "b", "check", "c", "clippy", "test", "t", "run", "r", "bench", "doc", "d", "install",
    "rustc", "rustdoc", "fix", "package", "publish", "miri",
];

/// Global options that take their value as the next argument.
const GLOBAL_WITH_VALUE: [&str; 4] = ["--config", "-Z", "-C", "--color"];

/// Index of the cargo subcommand (`+toolchain` and global options skipped).
pub fn subcommand_index(args: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a.starts_with('+') {
            i += 1;
        } else if a.starts_with('-') {
            i += if GLOBAL_WITH_VALUE.contains(&a) { 2 } else { 1 };
        } else {
            return Some(i);
        }
    }
    None
}

/// True when the user already chose a job count (before any `--`).
pub fn has_jobs_flag(args: &[String]) -> bool {
    for a in args {
        if a == "--" {
            return false;
        }
        if a == "-j" || a == "--jobs" || a.starts_with("--jobs=") {
            return true;
        }
        if let Some(rest) = a.strip_prefix("-j")
            && rest.starts_with(|c: char| c.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

/// Insert `-j <jobs>` right after the subcommand (`build -j 24 --release`).
/// Arguments are returned unchanged when the user gave their own `-j`, the
/// subcommand does not take one, or there is no subcommand.
pub fn insert_jobs(args: &[String], jobs: u32) -> Vec<String> {
    let Some(i) = subcommand_index(args) else {
        return args.to_vec();
    };
    if !JOBS_COMMANDS.contains(&args[i].as_str()) || has_jobs_flag(&args[i + 1..]) {
        return args.to_vec();
    }
    let mut out = args.to_vec();
    out.insert(i + 1, "-j".to_string());
    out.insert(i + 2, jobs.to_string());
    out
}

/// Local slots plus the free slots of every peer that reported a status.
pub fn farm_jobs(
    local_slots: u32,
    peers: &[(crate::proto::PeerInfo, Option<crate::proto::Status>)],
) -> u32 {
    let remote: u32 = peers
        .iter()
        .filter_map(|(_, s)| s.as_ref())
        .filter(|s| s.accepting)
        .map(|s| s.slots_total.saturating_sub(s.slots_busy))
        .sum();
    local_slots.saturating_add(remote).clamp(1, MAX_JOBS)
}

/// True for a program that is crabfarm itself: chaining to it would loop.
pub fn is_crabfarm(program: &str, me: Option<&Path>) -> bool {
    let p = Path::new(program);
    let stem = p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if stem == "crabfarm" {
        return true;
    }
    match (std::fs::canonicalize(p), me.map(std::fs::canonicalize)) {
        (Ok(a), Some(Ok(b))) => a == b,
        _ => false,
    }
}

/// `build.rustc-wrapper` of one config file. `Some("")` means the file sets it
/// to the empty string (which switches a wrapper from further out off).
fn wrapper_in_config(file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    let v = table
        .get("build")?
        .as_table()?
        .get("rustc-wrapper")?
        .as_str()?
        .to_string();
    // A path with a directory part is relative to the directory that holds
    // the `.cargo` folder (cargo reference, "config-relative paths").
    if v.contains(['/', '\\']) && !Path::new(&v).is_absolute() {
        let base = file.parent()?.parent()?;
        return Some(base.join(&v).to_string_lossy().into_owned());
    }
    Some(v)
}

/// `--config build.rustc-wrapper="x"` on the command line.
fn wrapper_in_args(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            break;
        }
        let value = if a == "--config" {
            i += 1;
            args.get(i).map(String::as_str)
        } else {
            a.strip_prefix("--config=")
        };
        if let Some(kv) = value
            && let Some((k, v)) = kv.split_once('=')
            && k.trim() == "build.rustc-wrapper"
        {
            let doc: toml::Table = toml::from_str(&format!("v = {}", v.trim())).ok()?;
            return doc.get("v")?.as_str().map(str::to_string);
        }
        i += 1;
    }
    None
}

/// The rustc wrapper that cargo would use without crabfarm, in cargo's order:
/// the `RUSTC_WRAPPER` variable, `--config build.rustc-wrapper=`, the
/// `CARGO_BUILD_RUSTC_WRAPPER` variable, then `build.rustc-wrapper` of the
/// `.cargo/config.toml` files from `cwd` up to the root (the nearest wins) and
/// of `<CARGO_HOME>/config.toml`. (`cargo config get` is nightly-only, so the
/// files are read here.) An empty value, as in cargo, means no wrapper; the
/// program that is crabfarm itself is never returned.
pub fn find_inner_wrapper(
    args: &[String],
    cwd: &Path,
    cargo_home: Option<&Path>,
    env: &dyn Fn(&str) -> Option<OsString>,
    me: Option<&Path>,
) -> Option<String> {
    let var = |n: &str| env(n).and_then(|v| v.into_string().ok());
    // A set but empty RUSTC_WRAPPER switches a configured wrapper off, as in cargo.
    let found: Option<String> = var("RUSTC_WRAPPER")
        .or_else(|| wrapper_in_args(args))
        .or_else(|| var("CARGO_BUILD_RUSTC_WRAPPER"))
        .or_else(|| {
            let mut dirs: Vec<PathBuf> = cwd.ancestors().map(|d| d.join(".cargo")).collect();
            dirs.extend(cargo_home.map(Path::to_path_buf));
            dirs.iter().find_map(|d| {
                ["config.toml", "config"]
                    .iter()
                    .find_map(|f| wrapper_in_config(&d.join(f)))
            })
        });
    found.filter(|w| !w.is_empty() && !is_crabfarm(w, me))
}

/// Ask the node for its status; `None` when it is not running.
pub fn query_status(data_dir: &std::path::Path) -> Option<Message> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        let mut conn = ipc::connect(data_dir).await.ok()?;
        conn.send(&Message::QueryStatus).await.ok()?;
        match tokio::time::timeout(Duration::from_secs(3), conn.recv()).await {
            Ok(Ok(Some(m @ Message::FarmStatus { .. }))) => Some(m),
            _ => None,
        }
    })
}

pub fn run(args: &[String]) -> i32 {
    let Ok(data_dir) = config::data_dir() else {
        eprintln!("crabfarm: cannot find the data directory");
        return 1;
    };
    let cfg = Config::load(&data_dir).unwrap_or_default();
    let mut cargo_args = args.to_vec();
    match query_status(&data_dir) {
        Some(Message::FarmStatus { peers, .. }) => {
            cargo_args = insert_jobs(args, farm_jobs(cfg.local_slots, &peers));
        }
        _ => {
            eprintln!(
                "crabfarm: node is not running, building locally. Start it with `crabfarm node` in another terminal."
            );
        }
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("crabfarm: cannot find my own executable: {e}");
            return 1;
        }
    };
    let mut cargo = Command::new("cargo");
    cargo.args(&cargo_args).env("RUSTC_WRAPPER", &exe);
    // A wrapper that was in place before (sccache, ...) keeps working for the
    // runs that happen on this machine (CF-19).
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().join(".cargo")));
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Some(inner) = find_inner_wrapper(
        args,
        &cwd,
        cargo_home.as_deref(),
        &|n| std::env::var_os(n),
        Some(&exe),
    ) {
        cargo.env(INNER_WRAPPER_ENV, inner);
    }
    match cargo.status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("crabfarm: could not run cargo: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{NodeId, PeerInfo, Status};

    fn a(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn jobs_go_right_after_the_subcommand() {
        assert_eq!(
            insert_jobs(&a(&["build", "--release"]), 24),
            a(&["build", "-j", "24", "--release"])
        );
        assert_eq!(
            insert_jobs(&a(&["+nightly", "test", "-p", "x"]), 8),
            a(&["+nightly", "test", "-j", "8", "-p", "x"])
        );
        assert_eq!(
            insert_jobs(&a(&["--config", "a=b", "check"]), 8),
            a(&["--config", "a=b", "check", "-j", "8"])
        );
        assert_eq!(
            insert_jobs(&a(&["-Z", "unstable", "build"]), 3),
            a(&["-Z", "unstable", "build", "-j", "3"])
        );
        assert_eq!(
            insert_jobs(&a(&["run", "--", "-j", "2"]), 9),
            a(&["run", "-j", "9", "--", "-j", "2"])
        );
    }

    #[test]
    fn user_jobs_and_other_commands_are_left_alone() {
        for args in [
            a(&["build", "-j", "4"]),
            a(&["build", "--jobs=4"]),
            a(&["build", "-j4"]),
            a(&["build", "--jobs", "4"]),
        ] {
            assert_eq!(insert_jobs(&args, 24), args);
        }
        assert_eq!(insert_jobs(&a(&["fmt"]), 24), a(&["fmt"]));
        assert_eq!(insert_jobs(&a(&["--version"]), 24), a(&["--version"]));
        assert_eq!(insert_jobs(&[], 24), Vec::<String>::new());
    }

    fn write_cfg(dir: &Path, text: &str) {
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        std::fs::write(dir.join(".cargo").join("config.toml"), text).unwrap();
    }

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    /// CF-19: the wrapper in place before crabfarm is found where cargo looks.
    #[test]
    fn a_configured_rustc_wrapper_is_found_in_cargos_order() {
        let t = tempfile::tempdir().unwrap();
        let proj = t.path().join("a").join("b");
        std::fs::create_dir_all(&proj).unwrap();
        let home = t.path().join("home");
        write_cfg(
            &home,
            "[build]
rustc-wrapper = \"from-home\"
",
        );
        let find = |env: &dyn Fn(&str) -> Option<OsString>, args: &[String]| {
            find_inner_wrapper(args, &proj, Some(&home.join(".cargo")), env, None)
        };
        // Only CARGO_HOME/config.toml.
        assert_eq!(find(&no_env, &[]), Some("from-home".into()));
        // A nearer config wins over a farther one.
        write_cfg(
            t.path(),
            "[build]
rustc-wrapper = \"far\"
",
        );
        assert_eq!(find(&no_env, &[]), Some("far".into()));
        write_cfg(
            &t.path().join("a"),
            "[build]
rustc-wrapper = \"near\"
",
        );
        assert_eq!(find(&no_env, &[]), Some("near".into()));
        // Relative paths with a directory part are relative to the folder that holds `.cargo`.
        write_cfg(
            &proj,
            "[build]
rustc-wrapper = \"tools/w\"
",
        );
        assert_eq!(
            find(&no_env, &[]),
            Some(proj.join("tools/w").to_string_lossy().into_owned())
        );
        // `--config`, the build env variable and RUSTC_WRAPPER, in that order of strength.
        let cli = a(&["build", "--config", "build.rustc-wrapper=\"cli\""]);
        assert_eq!(find(&no_env, &cli), Some("cli".into()));
        let build_env = |n: &str| (n == "CARGO_BUILD_RUSTC_WRAPPER").then(|| "benv".into());
        assert_eq!(find(&build_env, &[]), Some("benv".into()));
        assert_eq!(find(&build_env, &cli), Some("cli".into()));
        let both = |n: &str| match n {
            "RUSTC_WRAPPER" => Some("sccache".into()),
            "CARGO_BUILD_RUSTC_WRAPPER" => Some("benv".into()),
            _ => None,
        };
        assert_eq!(find(&both, &cli), Some("sccache".into()));
        // An empty value switches the wrapper off (variable or config file).
        let empty = |n: &str| (n == "RUSTC_WRAPPER").then(OsString::new);
        assert_eq!(find(&empty, &[]), None);
        write_cfg(
            &proj,
            "[build]
rustc-wrapper = \"\"
",
        );
        assert_eq!(find(&no_env, &[]), None);
        // Nothing configured, or a file that is not valid TOML: no wrapper.
        let empty_dir = tempfile::tempdir().unwrap();
        assert_eq!(
            find_inner_wrapper(&[], empty_dir.path(), None, &no_env, None),
            None
        );
        write_cfg(empty_dir.path(), "not = [valid");
        assert_eq!(
            find_inner_wrapper(&[], empty_dir.path(), None, &no_env, None),
            None
        );
    }

    /// crabfarm is never chained to itself, whatever the spelling.
    #[test]
    fn crabfarm_is_never_its_own_inner_wrapper() {
        let t = tempfile::tempdir().unwrap();
        let exe = t.path().join("tool.bin");
        std::fs::write(&exe, b"x").unwrap();
        let env_exe = |n: &str| (n == "RUSTC_WRAPPER").then(|| exe.clone().into_os_string());
        // The very file this process runs as.
        assert_eq!(
            find_inner_wrapper(&[], t.path(), None, &env_exe, Some(&exe)),
            None
        );
        // Another name for the same program, and the usual file names.
        for name in ["crabfarm", "crabfarm.exe", "/opt/bin/CrabFarm"] {
            let e = |n: &str| (n == "RUSTC_WRAPPER").then(|| name.into());
            assert_eq!(
                find_inner_wrapper(&[], t.path(), None, &e, None),
                None,
                "{name}"
            );
        }
        write_cfg(
            t.path(),
            "[build]
rustc-wrapper = \"crabfarm\"
",
        );
        assert_eq!(find_inner_wrapper(&[], t.path(), None, &no_env, None), None);
        // A different program is kept.
        let e = |n: &str| (n == "RUSTC_WRAPPER").then(|| "sccache".into());
        assert_eq!(
            find_inner_wrapper(&[], t.path(), None, &e, Some(&exe)),
            Some("sccache".into())
        );
    }

    fn peer(total: u32, busy: u32, accepting: bool) -> (PeerInfo, Option<Status>) {
        (
            PeerInfo {
                node_id: NodeId([1; 16]),
                node_name: "p".into(),
                addrs: vec![],
                last_seen: 0,
            },
            Some(Status {
                node_id: NodeId([1; 16]),
                node_name: "p".into(),
                host_triple: "h".into(),
                slots_total: total,
                slots_busy: busy,
                queue_len: 0,
                speed: 100,
                toolchains: vec![],
                targets: vec![],
                accepting,
            }),
        )
    }

    #[test]
    fn farm_jobs_adds_free_remote_slots_and_caps() {
        assert_eq!(farm_jobs(4, &[peer(8, 3, true), peer(16, 0, false)]), 9);
        assert_eq!(farm_jobs(4, &[(peer(8, 0, true).0, None)]), 4);
        assert_eq!(farm_jobs(100, &[peer(64, 0, true)]), MAX_JOBS);
        assert_eq!(farm_jobs(0, &[]), 1);
    }
}
