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
use std::collections::HashSet;
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
    let Ok(data_dir) = config::data_dir() else {
        return passthrough(rustc, args);
    };

    if args
        .iter()
        .any(|a| a == "-vV" || a == "--version" || a == "-V")
    {
        return version_probe(rustc, args, &data_dir);
    }
    // No node running: nothing to gain from classifying.
    if !ipc::port_path(&data_dir).exists() {
        return passthrough(rustc, args);
    }
    let Ok(expanded) = rustc_args::expand_argfiles(args) else {
        return passthrough(rustc, args);
    };
    let Ok(cwd) = std::env::current_dir() else {
        return passthrough(rustc, args);
    };
    let Some(tc) = toolchain_id(rustc, &data_dir, &cwd) else {
        return passthrough(rustc, args);
    };
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
    Path::new(program)
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("rustc"))
}

/// The command for a LOCAL rustc run: rustc itself, or rustc behind the
/// wrapper that was configured before crabfarm (`crabfarm cargo` stores it in
/// `CRABFARM_INNER_WRAPPER`, CF-19). Never crabfarm itself.
fn local_command(rustc: &str, args: &[String]) -> Command {
    let inner = std::env::var(crate::cargo_cmd::INNER_WRAPPER_ENV)
        .ok()
        .filter(|w| {
            !w.is_empty()
                && !crate::cargo_cmd::is_crabfarm(w, std::env::current_exe().ok().as_deref())
        });
    match inner {
        Some(w) => {
            let mut c = Command::new(w);
            c.arg(rustc).args(args);
            c
        }
        None => {
            let mut c = Command::new(rustc);
            c.args(args);
            c
        }
    }
}

/// Run rustc with inherited stdio.
fn passthrough(rustc: &str, args: &[String]) -> i32 {
    match local_command(rustc, args).status() {
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
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let rustup = std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_default();
    // A rustup proxy picks the toolchain from the environment, and without
    // RUSTUP_TOOLCHAIN from the directory.
    let dir = if rustup.is_empty() {
        cwd.to_string_lossy().into_owned()
    } else {
        String::new()
    };
    let text = format!("{}|{}|{}|{rustup}|{dir}", path.display(), meta.len(), mtime);
    Some(blake3::hash(text.as_bytes()).to_hex().to_string())
}

fn resolve_in_path(rustc: &str) -> Option<PathBuf> {
    let p = Path::new(rustc);
    if p.components().count() > 1 || p.is_absolute() {
        return Some(p.to_path_buf());
    }
    let exts: &[&str] = if cfg!(windows) {
        &["exe", "cmd", "bat"]
    } else {
        &[""]
    };
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

const PATH_ENV: [&str; 5] = [
    "OUT_DIR",
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_TARGET_TMPDIR",
    "CARGO_RUSTC_CURRENT_DIR",
];

/// DESIGN section 9: the variables a job gets from the submitter: the
/// allow-list of what cargo sets for rustc (`guard::design_env`) and the names
/// the user listed in `forward_env`. Nothing is added automatically (CF-18).
pub fn is_forwarded(name: &str, extra: &[String]) -> bool {
    crate::guard::design_env(name) || extra.iter().any(|e| e.eq_ignore_ascii_case(name))
}

fn is_path_env(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    PATH_ENV.contains(&up.as_str()) || up.starts_with("CARGO_BIN_EXE_")
}

/// Forwarded variables of `vars`; values of the path variables become client paths.
pub fn forwarded_env(
    vars: impl Iterator<Item = (String, String)>,
    extra: &[String],
) -> Vec<(String, EnvValue)> {
    let mut out: Vec<(String, EnvValue)> = vars
        .filter(|(k, _)| {
            is_forwarded(k, extra) && (crate::guard::extra_env_name_ok(k) || is_forwarded(k, &[]))
        })
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
    cand.out_dir
        .to_local()
        .join(format!("{}{}.d", cand.crate_name, extra_filename(args)))
}

/// Names recorded for `crate_name` that are set now and are not part of the
/// forwarded environment: the crate cannot be built on a peer.
fn blocked_env(
    data_dir: &Path,
    crate_name: &str,
    forwarded: &HashSet<String>,
    is_set: impl Fn(&str) -> bool,
) -> Vec<String> {
    crate::envlearn::recorded(data_dir, crate_name)
        .into_iter()
        .filter(|n| is_set(n) && !forwarded.contains(&n.to_ascii_uppercase()))
        .collect()
}

/// Record the env dependencies of the `.d` file the LOCAL rustc just wrote
/// (never one that came from a peer) so the crate stays local while they are
/// set (CF-18). Recording does not forward anything.
fn learn_env_deps(data_dir: &Path, crate_name: &str, dep_file: &Path, forwarded: &HashSet<String>) {
    let Ok(text) = std::fs::read_to_string(dep_file) else {
        return;
    };
    let missing = missing_env_deps(&text, forwarded, |n| std::env::var_os(n).is_some());
    if !missing.is_empty() {
        let _ = crate::envlearn::remember(data_dir, crate_name, &missing);
    }
}

/// Names a freshly written `.d` file lists as env dependencies that are set
/// here but were not part of the job's environment.
pub fn missing_env_deps(
    dep_info: &str,
    forwarded: &HashSet<String>,
    is_set: impl Fn(&str) -> bool,
) -> Vec<String> {
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
    Local {
        printed: bool,
        reason: Option<LocalReason>,
    },
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
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return passthrough(rustc, args);
    };
    let Ok(mut conn) = rt.block_on(ipc::connect(data_dir)) else {
        return passthrough(rustc, args);
    };
    let Some(cwd_client) = ClientPath::from_local(cwd, cwd) else {
        return passthrough(rustc, args);
    };

    // Only what cargo sets for rustc and what the user listed travels (CF-18).
    // Names recorded from an earlier local build of this crate are NOT added.
    let env = forwarded_env(std::env::vars(), &cfg.forward_env);
    let forwarded: HashSet<String> = env.iter().map(|(k, _)| k.to_ascii_uppercase()).collect();
    // A variable this crate was seen to read (from a `.d` file the local rustc
    // wrote) that is set now but would not reach the peer: build locally.
    let force_local = !blocked_env(data_dir, &cand.crate_name, &forwarded, |n| {
        std::env::var_os(n).is_some()
    })
    .is_empty();
    // Names beyond DESIGN section 9; the executor accepts only these.
    let extra_env: Vec<String> = env
        .iter()
        .map(|(k, _)| k.clone())
        .filter(|k| !crate::guard::design_env(k))
        .collect();
    // A job that stays local carries no environment at all.
    let (env, extra_env) = if force_local {
        (Vec::new(), Vec::new())
    } else {
        (env, extra_env)
    };
    let spec = JobSpec {
        job_id: JobId({
            let mut b = [0u8; 16];
            rand::fill(&mut b[..]);
            b
        }),
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
        force_local,
    };
    if rt.block_on(conn.send(&submit)).is_err() {
        return passthrough(rustc, args);
    }

    let registry = is_registry_crate(&cand.input, cargo_home().as_ref());
    let dep_file = dep_info_file(&cand, expanded);
    let mut buffered: Vec<(bool, String)> = Vec::new();
    let mut printed = false;
    let mut local_retry_allowed = true;
    // Artifact notices cargo has already seen for this job (CF-16): a local
    // rerun rewrites the files but must not announce them a second time.
    let mut relayed_notices: HashSet<(String, String)> = HashSet::new();
    // The dep-info showed an env dependency that was not forwarded: the result
    // will be rejected, so no further early notice is passed on.
    let mut stale_early = false;

    let verdict = loop {
        // The node sends a keepalive every few seconds while it works; silence
        // means it is hung or gone (CF-19): the farm side failed, build here.
        let msg = match rt.block_on(ipc::recv_within(&mut conn, ipc::NODE_SILENCE_LIMIT)) {
            Some(m) => m,
            None => {
                break Verdict::Local {
                    printed,
                    reason: None,
                };
            }
        };
        match msg {
            Message::Keepalive => {}
            Message::RunLocal { reason } => {
                break Verdict::Local {
                    printed,
                    reason: Some(reason),
                };
            }
            Message::Stderr { line } => match rewrite::artifact(&line) {
                // Metadata and dep-info notices are printed at once even when
                // the diagnostics of a path crate are buffered: the submitter
                // has already put their (clean) files in place, and cargo
                // starts dependents on the notice (CF-16).
                Some((file, emit)) if crate::submitter::is_early_emit(&emit) => {
                    if emit == "dep-info" && local_retry_allowed && !stale_early {
                        // The dep-info is written before the metadata. A variable
                        // it lists that was not forwarded means this result is
                        // rejected at `Exit`; the metadata may differ from a local
                        // one, so cargo must never see its notice.
                        let missing = std::fs::read_to_string(&dep_file)
                            .map(|d| {
                                missing_env_deps(&d, &forwarded, |n| std::env::var_os(n).is_some())
                            })
                            .unwrap_or_default();
                        // A peer-written `.d`: a reason to distrust this result,
                        // never a source of names to forward or remember.
                        if !missing.is_empty() {
                            stale_early = true;
                        }
                    }
                    if !stale_early {
                        relayed_notices.insert(notice_key(&file, &emit));
                        printed |= registry;
                        write_line(false, &line);
                    }
                }
                _ if registry => {
                    printed = true;
                    write_line(false, &line);
                }
                _ => buffered.push((false, line)),
            },
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
                    if rt
                        .block_on(conn.send(&Message::RetryLocal {
                            reason: LocalReason::RemoteCompileErrorRetry,
                        }))
                        .is_err()
                    {
                        break Verdict::Local {
                            printed: false,
                            reason: None,
                        };
                    }
                    local_retry_allowed = false;
                    continue;
                }
                if code == 0 && local_retry_allowed {
                    let missing = std::fs::read_to_string(&dep_file)
                        .map(|d| {
                            missing_env_deps(&d, &forwarded, |n| std::env::var_os(n).is_some())
                        })
                        .unwrap_or_default();
                    if !missing.is_empty() {
                        // Same: only the local rerun's own `.d` is learned from.
                        buffered.clear();
                        if rt
                            .block_on(conn.send(&Message::RetryLocal {
                                reason: LocalReason::EnvDependency,
                            }))
                            .is_err()
                        {
                            break Verdict::Local {
                                printed: false,
                                reason: None,
                            };
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
            _ => {
                break Verdict::Local {
                    printed,
                    reason: None,
                };
            }
        }
    };

    match verdict {
        Verdict::Exit(code) => code,
        Verdict::Local { printed, reason } => {
            if printed {
                eprintln!("crabfarm: remote job failed, rerunning locally");
            }
            // The dep-info at this path may be the peer's (the submitter moves
            // it into place early): remove it, so that what is read after the
            // run was certainly written by the local rustc.
            let _ = std::fs::remove_file(&dep_file);
            let code = passthrough_skipping(rustc, args, &relayed_notices);
            if code == 0 {
                learn_env_deps(data_dir, &cand.crate_name, &dep_file, &forwarded);
            }
            if reason.is_some() {
                // Releases the local slot the node reserved for this run.
                let _ = rt.block_on(conn.send(&Message::LocalDone {
                    code,
                    produces: cand.produces.clone(),
                }));
            }
            code
        }
    }
}

/// Identity of an artifact notice that survives path rewriting: its emit kind
/// and the file name (one file per kind exists per job).
fn notice_key(file: &str, emit: &str) -> (String, String) {
    let name = file.rsplit(['\\', '/']).next().unwrap_or(file);
    (name.to_string(), emit.to_string())
}

/// True for a line that announces an artifact in `skip`.
fn is_skipped_notice(line: &str, skip: &HashSet<(String, String)>) -> bool {
    rewrite::artifact(line).is_some_and(|(f, e)| skip.contains(&notice_key(&f, &e)))
}

/// `passthrough`, except that artifact notices cargo already got from the
/// remote attempt are dropped from rustc's stderr (cargo must see each notice
/// once: a repeated metadata notice would release dependents twice).
fn passthrough_skipping(rustc: &str, args: &[String], skip: &HashSet<(String, String)>) -> i32 {
    use std::io::BufRead;
    if skip.is_empty() {
        return passthrough(rustc, args);
    }
    let mut child = match local_command(rustc, args).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("crabfarm: could not run {rustc}: {e}");
            return 1;
        }
    };
    if let Some(err) = child.stderr.take() {
        let mut reader = std::io::BufReader::new(err);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let text = String::from_utf8_lossy(&buf);
            if is_skipped_notice(text.trim_end_matches(['\r', '\n']), skip) {
                continue;
            }
            // One write per line, bytes untouched.
            let _ = std::io::stderr().lock().write_all(&buf);
        }
    }
    match child.wait() {
        Ok(s) => s.code().unwrap_or(1),
        Err(_) => 1,
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
    fn a_local_rerun_does_not_repeat_notices_cargo_already_saw() {
        let mut skip = HashSet::new();
        skip.insert(notice_key(r"C:\t\deps\libx-1.rmeta", "metadata"));
        // Same file, spelled with the other separator by the rewriter.
        assert!(is_skipped_notice(
            r#"{"artifact":"C:/t/deps/libx-1.rmeta","emit":"metadata"}"#,
            &skip
        ));
        // The rlib was never announced, and diagnostics are untouched.
        assert!(!is_skipped_notice(
            r#"{"artifact":"C:/t/deps/libx-1.rlib","emit":"link"}"#,
            &skip
        ));
        assert!(!is_skipped_notice("warning: unused", &skip));
    }

    #[test]
    fn forwarding_follows_the_design_list() {
        let extra = vec![v("MY_TOKEN_FREE")];
        for n in [
            "CARGO_PKG_NAME",
            "CARGO_PKG_VERSION_MAJOR",
            "CARGO",
            "CARGO_MANIFEST_DIR",
            "CARGO_MANIFEST_PATH",
            "CARGO_MANIFEST_LINKS",
            "CARGO_CRATE_NAME",
            "CARGO_BIN_NAME",
            "CARGO_PRIMARY_PACKAGE",
            "CARGO_TARGET_TMPDIR",
            "CARGO_RUSTC_CURRENT_DIR",
            "CARGO_BIN_EXE_tool",
            "CARGO_CFG_TARGET_OS",
            "OUT_DIR",
            "RUSTC_BOOTSTRAP",
            "MY_TOKEN_FREE",
        ] {
            assert!(is_forwarded(n, &extra), "{n} must be forwarded");
        }
        for n in [
            "PATH",
            "HOME",
            "SystemRoot",
            "RUSTFLAGS",
            "CARGO_MAKEFLAGS",
            "USERPROFILE",
            // Credentials and settings of cargo itself (CF-18).
            "CARGO_REGISTRY_TOKEN",
            "CARGO_REGISTRIES_PRIVATE_TOKEN",
            "CARGO_HOME",
            "CARGO_NET_GIT_FETCH_WITH_CLI",
            "CARGO_PKG_SECRET",
            "CARGO_CFG_",
            "GITHUB_TOKEN",
            // Build-script variables that cargo does not pass to rustc.
            "DEP_FOO_ROOT",
            "TARGET",
            "HOST",
            "PROFILE",
            "NUM_JOBS",
            "OPT_LEVEL",
            "DEBUG",
        ] {
            assert!(!is_forwarded(n, &extra), "{n} must not be forwarded");
        }
    }

    /// CF-18, the first leak: every `CARGO_*` variable used to be forwarded.
    #[test]
    fn registry_credentials_never_enter_the_forwarded_environment() {
        let vars = vec![
            (v("CARGO_REGISTRY_TOKEN"), v("secret-1")),
            (v("CARGO_REGISTRIES_MY_TOKEN"), v("secret-2")),
            (v("GITHUB_TOKEN"), v("secret-3")),
            (v("CARGO_PKG_NAME"), v("x")),
        ];
        let env = forwarded_env(vars.into_iter(), &[]);
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["CARGO_PKG_NAME"]);
    }

    #[test]
    fn a_recorded_name_blocks_the_crate_only_while_it_is_set_and_not_forwarded() {
        let d = tempfile::tempdir().unwrap();
        crate::envlearn::remember(d.path(), "krate", &[v("MY_VAR")]).unwrap();
        let none: HashSet<String> = HashSet::new();
        let set = |n: &str| n == "MY_VAR";
        assert_eq!(
            blocked_env(d.path(), "krate", &none, set),
            vec![v("MY_VAR")]
        );
        // Not set here: nothing to go wrong on the peer.
        assert!(blocked_env(d.path(), "krate", &none, |_| false).is_empty());
        // The user forwards it: the peer gets it, so the crate may go remote.
        let fwd: HashSet<String> = [v("MY_VAR")].into_iter().collect();
        assert!(blocked_env(d.path(), "krate", &fwd, set).is_empty());
        assert!(blocked_env(d.path(), "other", &none, set).is_empty());
    }

    #[test]
    fn only_rustc_runs_remotely() {
        assert!(is_plain_rustc("rustc"));
        assert!(is_plain_rustc(
            r"C:\Users\me\.rustup\toolchains\stable\bin\rustc.exe"
        ));
        assert!(is_plain_rustc("/usr/bin/rustc"));
        for p in [
            "clippy-driver",
            r"C:\x\clippy-driver.exe",
            "/x/rustdoc",
            "rustc-wrapper",
            "cargo-clippy",
            "miri",
        ] {
            assert!(!is_plain_rustc(p), "{p}");
        }
    }

    #[test]
    fn configured_names_that_would_change_the_peer_are_not_forwarded() {
        let extra = vec![v("PATH"), v("LD_PRELOAD"), v("RUSTC_WRAPPER"), v("MY_FLAG")];
        let vars = vec![
            (v("PATH"), v("p")),
            (v("LD_PRELOAD"), v("l")),
            (v("RUSTC_WRAPPER"), v("w")),
            (v("MY_FLAG"), v("1")),
            (v("RUSTC_BOOTSTRAP"), v("1")),
        ];
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
        assert_eq!(
            get("CARGO_MANIFEST_DIR"),
            Some(EnvValue::Plain(v("relative/not/absolute")))
        );
        assert_eq!(get("PATH"), None);
    }

    #[test]
    fn extra_filename_spellings() {
        let a = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            extra_filename(&a(&["-C", "extra-filename=-abc", "x.rs"])),
            "-abc"
        );
        assert_eq!(extra_filename(&a(&["-Cextra-filename=-def"])), "-def");
        assert_eq!(
            extra_filename(&a(&["--codegen", "extra-filename=-g"])),
            "-g"
        );
        assert_eq!(extra_filename(&a(&["--codegen=extra-filename=-h"])), "-h");
        assert_eq!(extra_filename(&a(&["-C", "opt-level=3"])), "");
    }

    #[test]
    fn registry_detection_uses_cargo_home() {
        let home = ClientPath::parse(PathStyle::Posix, "/home/u/.cargo").unwrap();
        let reg = ClientPath::parse(
            PathStyle::Posix,
            "/home/u/.cargo/registry/src/x/serde/src/lib.rs",
        )
        .unwrap();
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
