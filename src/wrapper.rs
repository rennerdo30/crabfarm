//! Wrapper mode (DESIGN sections 2, 5, 9, 10): cargo calls
//! `crabfarm <rustc> <args...>` for every rustc invocation. The wrapper
//! classifies it, and either runs rustc itself (passthrough, always the
//! fallback) or hands the job to the local node.
//!
//! Every failure of the farm side ends in a local run, never in a build error.

use crate::config::{self, Config, RemoteIncremental};
use crate::ipc;
use crate::paths::host_style;
use crate::proto::{ClientPath, EnvValue, JobId, JobSpec, LocalReason, Message, ToolchainId};
use crate::rewrite;
use crate::rustc_args::{self, Classified, ClassifyOptions, RemoteCandidate};
use crate::toolchain;
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Run one wrapper invocation; the return value is the process exit code.
pub fn run(rustc: &str, args: &[String]) -> i32 {
    let debug = std::env::var_os("CRABFARM_LOG").is_some();
    // `clippy-driver` and any other compiler driver behind RUSTC_WRAPPER run
    // here: the peers only have `rustc`, so a remote run would lint nothing.
    if !is_plain_rustc(rustc) {
        if debug {
            eprintln!("crabfarm: local (not rustc)");
        }
        return passthrough(rustc, args);
    }
    let Ok(data_dir) = config::data_dir() else { return passthrough(rustc, args) };

    if args.iter().any(|a| a == "-vV" || a == "--version" || a == "-V") {
        return version_probe(rustc, args, &data_dir);
    }
    // No node running: nothing to gain from classifying.
    if !ipc::port_path(&data_dir).exists() {
        return passthrough(rustc, args);
    }
    let Ok(expanded) = rustc_args::expand_argfiles(args) else { return passthrough(rustc, args) };
    let Ok(cwd) = std::env::current_dir() else { return passthrough(rustc, args) };
    let Some(tc) = toolchain_id(rustc, &data_dir, &cwd) else { return passthrough(rustc, args) };
    let cfg = Config::load(&data_dir).unwrap_or_default();
    let opts = ClassifyOptions {
        host_triple: &tc.host,
        cwd: &cwd,
        style: host_style(),
        strip_incremental: cfg.remote_incremental == RemoteIncremental::Strip,
    };
    match rustc_args::classify(&expanded, &opts) {
        Classified::Local { why } => {
            if debug {
                eprintln!("crabfarm: local ({why})");
            }
            passthrough(rustc, args)
        }
        Classified::Remote(cand) => submit(rustc, args, cand, tc, &cfg, &data_dir, &cwd, &expanded),
    }
}

/// True when the program's file stem is `rustc` (also `rustc.exe`).
pub fn is_plain_rustc(program: &str) -> bool {
    Path::new(program).file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case("rustc"))
}

/// Run rustc with inherited stdio.
fn passthrough(rustc: &str, args: &[String]) -> i32 {
    match Command::new(rustc).args(args).status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("crabfarm: could not run {rustc}: {e}");
            1
        }
    }
}

// ------------------------------------------------------------ toolchain id

fn rustc_cache_key(rustc: &str, cwd: &Path) -> Option<String> {
    let path = resolve_in_path(rustc)?;
    let meta = std::fs::metadata(&path).ok()?;
    let mtime = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    let rustup = std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_default();
    // A rustup proxy picks the toolchain from the environment, and without
    // RUSTUP_TOOLCHAIN from the directory.
    let dir = if rustup.is_empty() { cwd.to_string_lossy().into_owned() } else { String::new() };
    let text = format!("{}|{}|{}|{rustup}|{dir}", path.display(), meta.len(), mtime);
    Some(blake3::hash(text.as_bytes()).to_hex().to_string())
}

fn resolve_in_path(rustc: &str) -> Option<PathBuf> {
    let p = Path::new(rustc);
    if p.components().count() > 1 || p.is_absolute() {
        return Some(p.to_path_buf());
    }
    let exts: &[&str] = if cfg!(windows) { &["exe", "cmd", "bat"] } else { &[""] };
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        for ext in exts {
            let mut c = dir.join(rustc);
            if !ext.is_empty() {
                c.set_extension(ext);
            }
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

/// `rustc -vV` of this compiler, cached on disk so the next invocations of the
/// same build do not spawn it again.
fn toolchain_id(rustc: &str, data_dir: &Path, cwd: &Path) -> Option<ToolchainId> {
    let key = rustc_cache_key(rustc, cwd)?;
    let file = data_dir.join("rustc-id").join(&key);
    if let Ok(text) = std::fs::read_to_string(&file)
        && let Ok(id) = toolchain::parse_version_verbose(&text)
    {
        return Some(id);
    }
    let out = capture_version(rustc, &[]).ok()?;
    store_version(&file, &out);
    toolchain::parse_version_verbose(&out).ok()
}

fn capture_version(rustc: &str, extra: &[String]) -> std::io::Result<String> {
    let mut cmd = Command::new(rustc);
    if extra.is_empty() {
        cmd.arg("-vV");
    } else {
        cmd.args(extra);
    }
    cmd.stdin(Stdio::null()).stderr(Stdio::inherit());
    let out = cmd.output()?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn store_version(file: &Path, text: &str) {
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = file.with_extension("tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, file);
    }
}

/// `rustc -vV` goes straight through; its output also fills the cache that
/// later invocations read.
fn version_probe(rustc: &str, args: &[String], data_dir: &Path) -> i32 {
    if args == ["-vV"] {
        let cwd = std::env::current_dir().unwrap_or_default();
        if let (Some(key), Ok(out)) = (rustc_cache_key(rustc, &cwd), capture_version(rustc, &[])) {
            if toolchain::parse_version_verbose(&out).is_ok() {
                store_version(&data_dir.join("rustc-id").join(key), &out);
            }
            let _ = std::io::stdout().write_all(out.as_bytes());
            return 0;
        }
    }
    passthrough(rustc, args)
}

// -------------------------------------------------------------- environment

const PATH_ENV: [&str; 5] = ["OUT_DIR", "CARGO_MANIFEST_DIR", "CARGO_MANIFEST_PATH", "CARGO_TARGET_TMPDIR", "CARGO_RUSTC_CURRENT_DIR"];

/// DESIGN section 9: the variables a job gets from the submitter.
pub fn is_forwarded(name: &str, extra: &[String]) -> bool {
    let up = name.to_ascii_uppercase();
    if up == "CARGO_MAKEFLAGS" {
        // Jobserver file descriptors of this machine mean nothing elsewhere.
        return false;
    }
    up == "CARGO"
        || up.starts_with("CARGO_")
        || up.starts_with("DEP_")
        || matches!(up.as_str(), "OUT_DIR" | "RUSTC_BOOTSTRAP" | "TARGET" | "HOST" | "PROFILE" | "NUM_JOBS" | "OPT_LEVEL" | "DEBUG")
        || extra.iter().any(|e| e.eq_ignore_ascii_case(name))
}

fn is_path_env(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    PATH_ENV.contains(&up.as_str()) || up.starts_with("CARGO_BIN_EXE_")
}

/// Forwarded variables of `vars`; values of the path variables become client paths.
pub fn forwarded_env(vars: impl Iterator<Item = (String, String)>, extra: &[String]) -> Vec<(String, EnvValue)> {
    let mut out: Vec<(String, EnvValue)> = vars
        .filter(|(k, _)| is_forwarded(k, extra) && (crate::guard::extra_env_name_ok(k) || is_forwarded(k, &[])))
        .map(|(k, v)| {
            let value = if is_path_env(&k) {
                match ClientPath::parse(host_style(), &v) {
                    Some(p) => EnvValue::Path(p),
                    None => EnvValue::Plain(v),
                }
            } else {
                EnvValue::Plain(v)
            };
            (k, value)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn learned_path(data_dir: &Path) -> PathBuf {
    data_dir.join("env_forward.toml")
}

/// Variables a crate's `.d` file listed as dependencies that were not
/// forwarded the first time; remembered so later runs forward them.
fn learned_env(data_dir: &Path, crate_name: &str) -> Vec<String> {
    let text = std::fs::read_to_string(learned_path(data_dir)).unwrap_or_default();
    let map: BTreeMap<String, Vec<String>> = toml::from_str(&text).unwrap_or_default();
    map.get(crate_name).cloned().unwrap_or_default()
}

fn remember_env(data_dir: &Path, crate_name: &str, names: &[String]) {
    let text = std::fs::read_to_string(learned_path(data_dir)).unwrap_or_default();
    let mut map: BTreeMap<String, Vec<String>> = toml::from_str(&text).unwrap_or_default();
    let e = map.entry(crate_name.to_string()).or_default();
    for n in names {
        if !e.contains(n) {
            e.push(n.clone());
        }
    }
    if let Ok(s) = toml::to_string(&map) {
        let tmp = learned_path(data_dir).with_extension("tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, learned_path(data_dir));
        }
    }
}

// ------------------------------------------------------------- crate kinds

/// True when the crate's source lies under `CARGO_HOME` (registry, git
/// checkouts): its errors are authoritative (DESIGN section 10).
pub fn is_registry_crate(input: &ClientPath, cargo_home: Option<&ClientPath>) -> bool {
    cargo_home.is_some_and(|h| input.starts_with(h))
}

fn cargo_home() -> Option<ClientPath> {
    let p = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().join(".cargo")))?;
    ClientPath::from_local(&p, Path::new("."))
}

/// `-C extra-filename=X` in any of the spellings cargo and users write.
pub fn extra_filename(args: &[String]) -> String {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let value = if a == "-C" || a == "--codegen" {
            args.get(i + 1).map(|s| s.as_str())
        } else if let Some(v) = a.strip_prefix("--codegen=") {
            Some(v)
        } else {
            a.strip_prefix("-C")
        };
        if let Some(v) = value
            && let Some(x) = v.strip_prefix("extra-filename=")
        {
            return x.to_string();
        }
        i += 1;
    }
    String::new()
}

/// Where rustc wrote the dep-info of this invocation.
fn dep_info_file(cand: &RemoteCandidate, args: &[String]) -> PathBuf {
    cand.out_dir.to_local().join(format!("{}{}.d", cand.crate_name, extra_filename(args)))
}

/// Names a freshly written `.d` file lists as env dependencies that are set
/// here but were not part of the job's environment.
pub fn missing_env_deps(dep_info: &str, forwarded: &HashSet<String>, is_set: impl Fn(&str) -> bool) -> Vec<String> {
    rewrite::env_deps(dep_info)
        .into_iter()
        .filter(|n| is_set(n) && !forwarded.contains(&n.to_ascii_uppercase()))
        .collect()
}

// ------------------------------------------------------------------ submit

/// How the remote run ended, from the wrapper's point of view.
enum Verdict {
    /// Finished; exit with this code.
    Exit(i32),
    /// Run rustc here. `printed` is true when remote stderr was already shown.
    Local { printed: bool, reason: Option<LocalReason> },
}

#[allow(clippy::too_many_arguments)]
fn submit(
    rustc: &str,
    args: &[String],
    cand: RemoteCandidate,
    tc: ToolchainId,
    cfg: &Config,
    data_dir: &Path,
    cwd: &Path,
    expanded: &[String],
) -> i32 {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return passthrough(rustc, args) };
    let Ok(mut conn) = rt.block_on(ipc::connect(data_dir)) else { return passthrough(rustc, args) };
    let Some(cwd_client) = ClientPath::from_local(cwd, cwd) else { return passthrough(rustc, args) };

    let mut extra = cfg.forward_env.clone();
    extra.extend(learned_env(data_dir, &cand.crate_name));
    let env = forwarded_env(std::env::vars(), &extra);
    // Names beyond DESIGN section 9; the executor accepts only these.
    let extra_env: Vec<String> = env.iter().map(|(k, _)| k.clone()).filter(|k| !is_forwarded(k, &[])).collect();
    let forwarded: HashSet<String> = env.iter().map(|(k, _)| k.to_ascii_uppercase()).collect();
    let spec = JobSpec {
        job_id: JobId({ let mut b = [0u8; 16]; rand::fill(&mut b[..]); b }),
        toolchain: tc,
        target: cand.target.clone(),
        args: cand.args.clone(),
        cwd: cwd_client,
        extra_env,
        env,
        inputs: Vec::new(),
        dirs: Vec::new(),
        out_dir: cand.out_dir.clone(),
        crate_name: cand.crate_name.clone(),
        no_retain: false,
        needs_host: cand.is_proc_macro,
    };
    let submit = Message::Submit {
        spec,
        rustc: rustc.to_string(),
        produces: cand.produces.clone(),
        externs: cand.externs.clone(),
        input: cand.input.clone(),
        search_dirs: cand.search_dirs.clone(),
        link_libs: cand.link_libs.clone(),
    };
    if rt.block_on(conn.send(&submit)).is_err() {
        return passthrough(rustc, args);
    }

    let registry = is_registry_crate(&cand.input, cargo_home().as_ref());
    let dep_file = dep_info_file(&cand, expanded);
    let mut buffered: Vec<(bool, String)> = Vec::new();
    let mut printed = false;
    let mut local_retry_allowed = true;

    let verdict = loop {
        let msg = match rt.block_on(conn.recv()) {
            Ok(Some(m)) => m,
            // The node went away mid-job: the farm side failed, build here.
            Ok(None) | Err(_) => break Verdict::Local { printed, reason: None },
        };
        match msg {
            Message::RunLocal { reason } => break Verdict::Local { printed, reason: Some(reason) },
            Message::Stderr { line } => {
                if registry {
                    printed = true;
                    write_line(false, &line);
                } else {
                    buffered.push((false, line));
                }
            }
            Message::Stdout { line } => {
                if registry {
                    printed = true;
                    write_line(true, &line);
                } else {
                    buffered.push((true, line));
                }
            }
            Message::Exit { code } => {
                if code != 0 && !registry && local_retry_allowed {
                    // A path or git crate may fail remotely for reasons that
                    // are not real (inputs outside the package directory):
                    // judge it by a local run (DESIGN section 10).
                    buffered.clear();
                    if rt.block_on(conn.send(&Message::RetryLocal { reason: LocalReason::RemoteCompileErrorRetry })).is_err() {
                        break Verdict::Local { printed: false, reason: None };
                    }
                    local_retry_allowed = false;
                    continue;
                }
                if code == 0 && local_retry_allowed {
                    let missing = std::fs::read_to_string(&dep_file)
                        .map(|d| missing_env_deps(&d, &forwarded, |n| std::env::var_os(n).is_some()))
                        .unwrap_or_default();
                    if !missing.is_empty() {
                        remember_env(data_dir, &cand.crate_name, &missing);
                        buffered.clear();
                        if rt.block_on(conn.send(&Message::RetryLocal { reason: LocalReason::EnvDependency })).is_err() {
                            break Verdict::Local { printed: false, reason: None };
                        }
                        local_retry_allowed = false;
                        continue;
                    }
                }
                for (out, line) in buffered.drain(..) {
                    write_line(out, &line);
                }
                break Verdict::Exit(code);
            }
            // A node that answers with anything else is not one to rely on.
            _ => break Verdict::Local { printed, reason: None },
        }
    };

    match verdict {
        Verdict::Exit(code) => code,
        Verdict::Local { printed, reason } => {
            if printed {
                eprintln!("crabfarm: remote job failed, rerunning locally");
            }
            let code = passthrough(rustc, args);
            if reason.is_some() {
                // Releases the local slot the node reserved for this run.
                let _ = rt.block_on(conn.send(&Message::LocalDone { code, produces: cand.produces.clone() }));
            }
            code
        }
    }
}

fn write_line(stdout: bool, line: &str) {
    // One write per line so lines of parallel wrappers never interleave inside a line.
    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    if stdout {
        let _ = std::io::stdout().lock().write_all(&buf);
        let _ = std::io::stdout().flush();
    } else {
        let _ = std::io::stderr().lock().write_all(&buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::PathStyle;

    fn v(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn forwarding_follows_the_design_list() {
        let extra = vec![v("MY_TOKEN_FREE")];
        for n in ["CARGO_PKG_NAME", "CARGO", "DEP_FOO_ROOT", "OUT_DIR", "RUSTC_BOOTSTRAP", "TARGET", "HOST", "PROFILE", "NUM_JOBS", "OPT_LEVEL", "DEBUG", "MY_TOKEN_FREE"] {
            assert!(is_forwarded(n, &extra), "{n} must be forwarded");
        }
        for n in ["PATH", "HOME", "SystemRoot", "RUSTFLAGS", "CARGO_MAKEFLAGS", "USERPROFILE"] {
            assert!(!is_forwarded(n, &extra), "{n} must not be forwarded");
        }
    }

    #[test]
    fn only_rustc_runs_remotely() {
        assert!(is_plain_rustc("rustc"));
        assert!(is_plain_rustc(r"C:\Users\me\.rustup\toolchains\stable\bin\rustc.exe"));
        assert!(is_plain_rustc("/usr/bin/rustc"));
        for p in ["clippy-driver", r"C:\x\clippy-driver.exe", "/x/rustdoc", "rustc-wrapper", "cargo-clippy", "miri"] {
            assert!(!is_plain_rustc(p), "{p}");
        }
    }

    #[test]
    fn configured_names_that_would_change_the_peer_are_not_forwarded() {
        let extra = vec![v("PATH"), v("LD_PRELOAD"), v("RUSTC_WRAPPER"), v("MY_FLAG")];
        let vars = vec![(v("PATH"), v("p")), (v("LD_PRELOAD"), v("l")), (v("RUSTC_WRAPPER"), v("w")), (v("MY_FLAG"), v("1")), (v("RUSTC_BOOTSTRAP"), v("1"))];
        let env = forwarded_env(vars.into_iter(), &extra);
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["MY_FLAG", "RUSTC_BOOTSTRAP"]);
    }

    #[test]
    fn path_variables_become_client_paths() {
        let dir = std::env::temp_dir();
        let d = dir.to_string_lossy().into_owned();
        let vars = vec![
            (v("OUT_DIR"), d.clone()),
            (v("CARGO_PKG_NAME"), v("x")),
            (v("CARGO_MANIFEST_DIR"), v("relative/not/absolute")),
            (v("PATH"), v("ignored")),
        ];
        let env = forwarded_env(vars.into_iter(), &[]);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert!(matches!(get("OUT_DIR"), Some(EnvValue::Path(_))));
        assert_eq!(get("CARGO_PKG_NAME"), Some(EnvValue::Plain(v("x"))));
        assert_eq!(get("CARGO_MANIFEST_DIR"), Some(EnvValue::Plain(v("relative/not/absolute"))));
        assert_eq!(get("PATH"), None);
    }

    #[test]
    fn extra_filename_spellings() {
        let a = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(extra_filename(&a(&["-C", "extra-filename=-abc", "x.rs"])), "-abc");
        assert_eq!(extra_filename(&a(&["-Cextra-filename=-def"])), "-def");
        assert_eq!(extra_filename(&a(&["--codegen", "extra-filename=-g"])), "-g");
        assert_eq!(extra_filename(&a(&["--codegen=extra-filename=-h"])), "-h");
        assert_eq!(extra_filename(&a(&["-C", "opt-level=3"])), "");
    }

    #[test]
    fn registry_detection_uses_cargo_home() {
        let home = ClientPath::parse(PathStyle::Posix, "/home/u/.cargo").unwrap();
        let reg = ClientPath::parse(PathStyle::Posix, "/home/u/.cargo/registry/src/x/serde/src/lib.rs").unwrap();
        let own = ClientPath::parse(PathStyle::Posix, "/home/u/proj/src/lib.rs").unwrap();
        assert!(is_registry_crate(&reg, Some(&home)));
        assert!(!is_registry_crate(&own, Some(&home)));
        assert!(!is_registry_crate(&reg, None));
    }

    #[test]
    fn env_dependencies_need_a_local_value_and_no_forwarding() {
        let d = "x.rs: y.rs\n# env-dep:FOO\n# env-dep:BAR=1\n# env-dep:CARGO_PKG_NAME=x\n";
        let forwarded: HashSet<String> = ["CARGO_PKG_NAME".to_string()].into_iter().collect();
        let set = |n: &str| n == "FOO" || n == "CARGO_PKG_NAME";
        assert_eq!(missing_env_deps(d, &forwarded, set), vec![v("FOO")]);
    }
}
