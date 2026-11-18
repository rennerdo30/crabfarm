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
    // An earlier wrapper found the node silent a moment ago: do not pay the
    // 30 s wait again for every rustc call while it is frozen (CF-25).
    if ipc::unresponsive_until(&data_dir).is_some() {
        if debug {
            eprintln!("crabfarm: local (the node did not answer recently)");
        }
        return passthrough(rustc, args);
    }
    let Ok(expanded) = rustc_args::expand_argfiles(args) else {
        return passthrough(rustc, args);
    };
    let Ok(cwd) = std::env::current_dir() else {
        return passthrough(rustc, args);
    };
    let cwd = logical_cwd(cwd);
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

/// The working directory spelled the way cargo spells its own paths (CF-29).
/// `current_dir` returns the real path, while cargo's `CARGO_MANIFEST_DIR` and
/// `--out-dir` keep the symlinked spelling (macOS temp and home folders live
/// under `/var` and `/private/var`). A relative crate path joined onto the real
/// cwd then names a different spelling than the package files collected under
/// the manifest dir, and the peer finds no source file. When an ancestor of the
/// manifest dir is the same directory as the cwd, that ancestor is used. Only
/// Unix has such spellings; elsewhere the cwd is returned unchanged.
fn logical_cwd(cwd: PathBuf) -> PathBuf {
    if !cfg!(unix) {
        return cwd;
    }
    let Some(manifest) = std::env::var_os("CARGO_MANIFEST_DIR") else {
        return cwd;
    };
    for anc in Path::new(&manifest).ancestors() {
        if anc.is_absolute() && std::fs::canonicalize(anc).is_ok_and(|c| c == cwd) {
            return anc.to_path_buf();
        }
    }
    cwd
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
    crate::guard::design_env(name) || extra.iter().any(|e| crate::guard::env_name_eq(e, name))
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

// ------------------------------------------------------ build-script output

/// Names a build script asked cargo to set for rustc (CF-21).
///
/// Syntax, from the cargo book, "Build Scripts" > "Outputs of the Build Script"
/// (doc.rust-lang.org/cargo/reference/build-scripts.html#outputs-of-the-build-script):
/// `cargo::rustc-env=VAR=VALUE` (and the older single-colon spelling
/// `cargo:rustc-env=VAR=VALUE`) sets `VAR` to `VALUE` for the crate's rustc and
/// `env!`. Only the NAME is taken: the value is whatever cargo put into this
/// wrapper's own environment, so nothing read from the file is ever sent.
///
/// The text comes from a file in the user's target directory, so it is capped
/// (names, count) and every name is validated like the executor does
/// (`guard::build_env_name_ok`); malformed lines are ignored.
pub fn parse_build_env_names(stdout: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let rest = line
            .strip_prefix("cargo::rustc-env=")
            .or_else(|| line.strip_prefix("cargo:rustc-env="));
        let Some((name, _value)) = rest.and_then(|r| r.split_once('=')) else {
            continue;
        };
        if crate::guard::build_env_name_ok(name) && !out.iter().any(|n| n == name) {
            out.push(name.to_string());
            if out.len() == crate::guard::MAX_BUILD_ENV {
                break;
            }
        }
    }
    out
}

/// The output file cargo wrote for the build script run of the package whose
/// `OUT_DIR` is `out_dir`, or `None` when it is not there. Layouts:
/// - current: `<build>/<pkg>/<hash>/out`, output in `<build>/<pkg>/<hash>/run/stdout`;
/// - older: `<build>/<pkg>-<hash>/out`, output in `<build>/<pkg>-<hash>/output`.
///
/// Both files are written by cargo from the build script's stdout. Nothing is
/// guessed: any other shape of `OUT_DIR` yields `None`.
pub fn build_script_output_file(out_dir: &Path) -> Option<PathBuf> {
    if out_dir.file_name()? != "out" {
        return None;
    }
    let unit = out_dir.parent()?;
    [unit.join("run").join("stdout"), unit.join("output")]
        .into_iter()
        .find(|p| p.is_file())
}

/// Largest build-script output read; anything bigger is ignored.
const MAX_BUILD_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;

/// Names the local build-script output declares with `rustc-env`. `None` when
/// the file cannot be found or read while `OUT_DIR` is set.
fn build_env_names_from_out_dir(out_dir: &Path) -> Option<Vec<String>> {
    let file = build_script_output_file(out_dir)?;
    if std::fs::metadata(&file).ok()?.len() > MAX_BUILD_OUTPUT_BYTES {
        return None;
    }
    let bytes = std::fs::read(&file).ok()?;
    Some(parse_build_env_names(&String::from_utf8_lossy(&bytes)))
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
/// True when the dep-info was read and lists no such variable.
fn learn_env_deps(
    data_dir: &Path,
    crate_name: &str,
    dep_file: &Path,
    forwarded: &HashSet<String>,
) -> bool {
    learn_env_deps_with(data_dir, crate_name, dep_file, forwarded, |n| {
        std::env::var_os(n).is_some()
    })
}

fn learn_env_deps_with(
    data_dir: &Path,
    crate_name: &str,
    dep_file: &Path,
    forwarded: &HashSet<String>,
    is_set: impl Fn(&str) -> bool,
) -> bool {
    let Ok(text) = std::fs::read_to_string(dep_file) else {
        return false;
    };
    let missing = missing_env_deps(&text, forwarded, is_set);
    if !missing.is_empty() {
        let _ = crate::envlearn::remember(data_dir, crate_name, &missing);
    }
    missing.is_empty()
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
    /// Run rustc here (nothing of the remote diagnostics was printed).
    Local { reason: Option<LocalReason> },
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
    //
    // Names the crate's own build script emitted as `rustc-env` (CF-21) travel
    // too, found in the output cargo wrote locally for this package.
    let build_names = match std::env::var_os("OUT_DIR") {
        Some(out_dir) => {
            let names = build_env_names_from_out_dir(Path::new(&out_dir));
            if names.is_none() && std::env::var_os("CRABFARM_LOG").is_some() {
                // Not guessed: a missing name makes the remote compile fail and
                // the local rerun (DESIGN section 10) builds the crate.
                eprintln!(
                    "crabfarm: stat build_script_output_missing {}",
                    cand.crate_name
                );
            }
            names.unwrap_or_default()
        }
        None => Vec::new(),
    };
    let mut allowed = cfg.forward_env.clone();
    allowed.extend(build_names.iter().cloned());
    let env = forwarded_env(std::env::vars(), &allowed);
    let forwarded: HashSet<String> = env.iter().map(|(k, _)| k.to_ascii_uppercase()).collect();
    // A variable this crate was seen to read (from a `.d` file the local rustc
    // wrote) that is set now but would not reach the peer: build locally.
    let force_local = !blocked_env(data_dir, &cand.crate_name, &forwarded, |n| {
        std::env::var_os(n).is_some()
    })
    .is_empty();
    // Names beyond DESIGN section 9; the executor accepts only these. A name the
    // build script declared is `build_env` (source-stamped), everything else
    // beyond the allow-list came from `forward_env`.
    let is_build = |k: &str| build_names.iter().any(|b| crate::guard::env_name_eq(b, k));
    let beyond: Vec<String> = env
        .iter()
        .map(|(k, _)| k.clone())
        .filter(|k| !crate::guard::design_env(k))
        .collect();
    let build_env: Vec<String> = beyond.iter().filter(|k| is_build(k)).cloned().collect();
    let extra_env: Vec<String> = beyond.into_iter().filter(|k| !is_build(k)).collect();
    // A job that stays local carries no environment at all.
    let (env, extra_env, build_env) = if force_local {
        (Vec::new(), Vec::new(), Vec::new())
    } else {
        (env, extra_env, build_env)
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
        build_env,
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

    let dep_file = dep_info_file(&cand, expanded);
    // Diagnostics of every crate stay buffered until the remote result is known
    // (CF-21): any remote failure is judged by a local run, registry crates
    // included (they compile with `--cap-lints allow`, so little is buffered).
    let mut buffered: Vec<(bool, String)> = Vec::new();
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
                // Silence or a lost connection: tell the wrappers that follow.
                ipc::mark_unresponsive(data_dir);
                break Verdict::Local { reason: None };
            }
        };
        match msg {
            Message::Keepalive => {}
            Message::RunLocal { reason } => {
                break Verdict::Local {
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
                        write_line(false, &line);
                    }
                }
                _ => buffered.push((false, line)),
            },
            Message::Stdout { line } => buffered.push((true, line)),
            Message::Exit { code } => {
                if code != 0 && local_retry_allowed {
                    // A crate may fail remotely for reasons that are not real
                    // (inputs outside the package directory, an environment
                    // variable that did not travel): judge it by a local run,
                    // for registry and path crates alike (DESIGN section 10).
                    buffered.clear();
                    if rt
                        .block_on(conn.send(&Message::RetryLocal {
                            reason: LocalReason::RemoteCompileErrorRetry,
                        }))
                        .is_err()
                    {
                        break Verdict::Local { reason: None };
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
                            break Verdict::Local { reason: None };
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
                break Verdict::Local { reason: None };
            }
        }
    };

    match verdict {
        Verdict::Exit(code) => code,
        Verdict::Local { reason } => {
            // The dep-info at this path may be the peer's (the submitter moves
            // it into place early): remove it, so that what is read after the
            // run was certainly written by the local rustc.
            let _ = std::fs::remove_file(&dep_file);
            // No `RunLocal`: the node did not hand over a slot, so it is silent,
            // hung or gone. Close the connection BEFORE rustc starts (CF-25):
            // a node that was only stalled and resumes sees the closed
            // connection and writes no remote output onto the files this local
            // rustc is about to write.
            let mut conn = if reason.is_some() {
                Some(conn)
            } else {
                rt.block_on(close_ipc(conn));
                None
            };
            let (code, printed) = run_local_capturing(rustc, args, &relayed_notices);
            // The compile cache (DESIGN section 16) may keep this run only when
            // it read nothing the cache key does not cover: no variable the
            // dep-info lists was set here without being part of the job.
            let mut cacheable = false;
            if code == 0 {
                cacheable = learn_env_deps(data_dir, &cand.crate_name, &dep_file, &forwarded);
            }
            if let Some(conn) = conn.as_mut() {
                // Releases the local slot the node reserved for this run.
                let _ = rt.block_on(conn.send(&Message::LocalDone {
                    code,
                    produces: cand.produces.clone(),
                    capture: (cacheable && cfg.cache).then_some(printed).flatten(),
                }));
            }
            code
        }
    }
}

/// Shut the IPC connection down and drop it, so the node sees end-of-file now
/// (bounded: a socket that will not close must not hold the build up).
async fn close_ipc(mut conn: crate::transport::FramedConn<tokio::net::TcpStream>) {
    use tokio::io::AsyncWriteExt;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), conn.stream.shutdown()).await;
    drop(conn);
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

/// Everything rustc printed, kept while it is passed on.
struct Tee {
    lines: Vec<(bool, String)>,
    bytes: usize,
    /// Every line was valid text and the total stayed within the cache's limit.
    valid: bool,
}

/// Pass the lines of one of rustc's streams on, byte for byte and one write
/// per line, and keep them in `tee`. Artifact notices in `skip` are dropped.
fn pump(
    mut reader: impl std::io::BufRead,
    stdout: bool,
    skip: &HashSet<(String, String)>,
    tee: &std::sync::Mutex<Tee>,
) {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&buf);
        let line = text.trim_end_matches(['\r', '\n']);
        let skipped = !skip.is_empty() && is_skipped_notice(line, skip);
        {
            let mut t = tee.lock().unwrap_or_else(|e| e.into_inner());
            t.bytes += buf.len();
            if t.bytes > crate::ccache::MAX_LINES_BYTES || std::str::from_utf8(&buf).is_err() {
                t.valid = false;
            } else if t.valid {
                t.lines.push((stdout, line.to_string()));
            }
        }
        // Kept for the cache either way: a run replayed from it prints every
        // notice, as a whole run does.
        if skipped {
            continue;
        }
        if stdout {
            let mut o = std::io::stdout().lock();
            let _ = o.write_all(&buf);
            let _ = o.flush();
        } else {
            let _ = std::io::stderr().lock().write_all(&buf);
        }
    }
}

/// Run rustc locally and pass its output on as it comes, like inherited
/// stdio would, while keeping it for the compile cache. Artifact notices cargo
/// already got from the remote attempt (`skip`) are dropped: cargo must see
/// each notice once, a repeated metadata notice would release dependents
/// twice; the capture still holds them. The capture is `None` when the output
/// cannot be replayed exactly (not text, too long).
fn run_local_capturing(
    rustc: &str,
    args: &[String],
    skip: &HashSet<(String, String)>,
) -> (i32, Option<crate::proto::LocalCapture>) {
    let mut cmd = local_command(rustc, args);
    let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("crabfarm: could not run {rustc}: {e}");
            return (1, None);
        }
    };
    let tee = std::sync::Mutex::new(Tee {
        lines: Vec::new(),
        bytes: 0,
        valid: true,
    });
    std::thread::scope(|sc| {
        if let Some(out) = child.stdout.take() {
            let tee = &tee;
            sc.spawn(move || pump(std::io::BufReader::new(out), true, &HashSet::new(), tee));
        }
        if let Some(err) = child.stderr.take() {
            pump(std::io::BufReader::new(err), false, skip, &tee);
        }
    });
    let code = match child.wait() {
        Ok(s) => s.code().unwrap_or(1),
        Err(_) => 1,
    };
    let t = tee.into_inner().unwrap_or_else(|e| e.into_inner());
    let capture = t
        .valid
        .then_some(crate::proto::LocalCapture { lines: t.lines });
    (code, capture)
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

    fn v(s: &str) -> String {
        s.to_string()
    }

    /// CF-25: `forward_env` names match the environment the way the OS does:
    /// case-insensitively on Windows, exactly elsewhere.
    #[test]
    fn forward_env_names_follow_the_case_rules_of_the_os() {
        let extra = vec!["My_Var".to_string()];
        assert!(is_forwarded("My_Var", &extra));
        assert_eq!(is_forwarded("MY_VAR", &extra), cfg!(windows));
        assert_eq!(is_forwarded("my_var", &extra), cfg!(windows));
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
            // CF-28: rustc reads these for Apple targets; they are in the cache key.
            "MACOSX_DEPLOYMENT_TARGET",
            "IPHONEOS_DEPLOYMENT_TARGET",
            "TVOS_DEPLOYMENT_TARGET",
            "WATCHOS_DEPLOYMENT_TARGET",
            "XROS_DEPLOYMENT_TARGET",
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
        // A drive-letter path is a path only on Windows.
        #[cfg(windows)]
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
    fn build_script_output_names_both_syntaxes() {
        let out = "\
cargo:rerun-if-changed=build.rs\n\
cargo:rustc-env=CF_BUILT=yes\n\
cargo::rustc-env=CF_NEW=a=b\n\
cargo:rustc-cfg=foo\n\
cargo:rustc-env=CF_BUILT=again\r\n\
cargo::rustc-env=CF_CRLF=1\r\n";
        assert_eq!(
            parse_build_env_names(out),
            vec![v("CF_BUILT"), v("CF_NEW"), v("CF_CRLF")]
        );
    }

    #[test]
    fn build_script_output_malformed_lines_are_ignored() {
        let out = "\
cargo:rustc-env=NOVALUE\n\
cargo:rustc-env==x\n\
cargo:rustc-env=1BAD=x\n\
cargo:rustc-env=A-B=x\n\
cargo:rustc-env=PATH=/evil\n\
cargo:rustc-env=RUSTC_WRAPPER=x\n\
cargo:rustc-env=CARGO_HOME=x\n\
cargo:rustc-env=LD_PRELOAD=x\n\
\x20cargo:rustc-env=INDENTED=x\n\
cargo:rustc-envFOO=x\n\
cargo:rustc-cdylib-link-arg=X=1\n\
cargo:warning=rustc-env=W=1\n\
cargo:rustc-env=OK_NAME=\n";
        assert_eq!(parse_build_env_names(out), vec![v("OK_NAME")]);
    }

    #[test]
    fn build_script_output_caps_names() {
        let long = "A".repeat(crate::guard::MAX_BUILD_ENV_NAME + 1);
        let fits = "B".repeat(crate::guard::MAX_BUILD_ENV_NAME);
        let mut out = format!("cargo:rustc-env={long}=x\ncargo:rustc-env={fits}=x\n");
        for i in 0..200 {
            out.push_str(&format!("cargo:rustc-env=V{i}=x\n"));
        }
        let names = parse_build_env_names(&out);
        assert_eq!(names.len(), crate::guard::MAX_BUILD_ENV);
        assert_eq!(names[0], fits);
        assert!(!names.contains(&long));
    }

    #[test]
    fn build_script_output_is_found_in_both_cargo_layouts() {
        let t = tempfile::tempdir().unwrap();
        // Current layout: <build>/<pkg>/<hash>/out with run/stdout.
        let unit = t.path().join("pkg").join("abc123");
        std::fs::create_dir_all(unit.join("out")).unwrap();
        std::fs::create_dir_all(unit.join("run")).unwrap();
        assert_eq!(build_script_output_file(&unit.join("out")), None);
        std::fs::write(unit.join("run").join("stdout"), "cargo:rustc-env=NEW_L=1\n").unwrap();
        assert_eq!(
            build_script_output_file(&unit.join("out")),
            Some(unit.join("run").join("stdout"))
        );
        assert_eq!(
            build_env_names_from_out_dir(&unit.join("out")),
            Some(vec![v("NEW_L")])
        );
        // Older layout: <build>/<pkg>-<hash>/out with `output`.
        let old = t.path().join("pkg-def456");
        std::fs::create_dir_all(old.join("out")).unwrap();
        assert_eq!(build_script_output_file(&old.join("out")), None);
        std::fs::write(old.join("output"), "cargo:rustc-env=OLD_L=1\n").unwrap();
        assert_eq!(
            build_script_output_file(&old.join("out")),
            Some(old.join("output"))
        );
        assert_eq!(
            build_env_names_from_out_dir(&old.join("out")),
            Some(vec![v("OLD_L")])
        );
        // An OUT_DIR of any other shape is not guessed at.
        assert_eq!(build_script_output_file(&unit), None);
        assert_eq!(build_script_output_file(&t.path().join("elsewhere")), None);
    }

    #[test]
    fn env_dependencies_need_a_local_value_and_no_forwarding() {
        let d = "x.rs: y.rs\n# env-dep:FOO\n# env-dep:BAR=1\n# env-dep:CARGO_PKG_NAME=x\n";
        let forwarded: HashSet<String> = ["CARGO_PKG_NAME".to_string()].into_iter().collect();
        let set = |n: &str| n == "FOO" || n == "CARGO_PKG_NAME";
        assert_eq!(missing_env_deps(d, &forwarded, set), vec![v("FOO")]);
    }

    /// CF-28: a local run may enter the compile cache only when the dep-info
    /// shows that no variable outside the job's environment was read. A result
    /// compiled with `FOO=1` must not be served to a build without it.
    #[test]
    fn a_local_run_that_read_an_unforwarded_variable_is_not_cacheable() {
        let d = tempfile::tempdir().unwrap();
        let dep = d.path().join("x.d");
        std::fs::write(&dep, "x.rmeta: a.rs\n\na.rs:\n\n# env-dep:FOO=1\n").unwrap();
        let none = HashSet::new();
        let fwd: HashSet<String> = ["FOO".to_string()].into();
        // FOO is set and not part of the job: not cacheable (and recorded).
        assert!(!learn_env_deps_with(d.path(), "x", &dep, &none, |n| n == "FOO"));
        // FOO is not set here: what was compiled is what any build without it gets.
        assert!(learn_env_deps_with(d.path(), "x", &dep, &none, |_| false));
        // FOO travels with the job, so it is in the cache key.
        assert!(learn_env_deps_with(d.path(), "x", &dep, &fwd, |n| n == "FOO"));
        // No dep-info at all: nothing to judge by.
        assert!(!learn_env_deps_with(
            d.path(),
            "x",
            &d.path().join("missing.d"),
            &none,
            |_| false
        ));
    }

    /// CF-28: what rustc prints is passed on line by line AND kept; a notice
    /// cargo already saw is kept but not passed on again; text that is not
    /// valid UTF-8 makes the run uncacheable.
    #[test]
    fn the_tee_keeps_every_line_and_flags_what_cannot_be_replayed() {
        let tee = |input: &[u8], skip: &HashSet<(String, String)>| {
            let t = std::sync::Mutex::new(Tee {
                lines: Vec::new(),
                bytes: 0,
                valid: true,
            });
            pump(std::io::BufReader::new(input), false, skip, &t);
            t.into_inner().unwrap()
        };
        let notice = r#"{"artifact":"C:\\o\\libx-1.rmeta","emit":"metadata"}"#;
        let input = format!("warning: a\r\n{notice}\nlast without newline");
        let mut skip = HashSet::new();
        skip.insert(notice_key(r"C:\o\libx-1.rmeta", "metadata"));
        let t = tee(input.as_bytes(), &skip);
        assert!(t.valid);
        assert_eq!(
            t.lines,
            vec![
                (false, "warning: a".to_string()),
                (false, notice.to_string()),
                (false, "last without newline".to_string())
            ]
        );
        assert!(!tee(b"ok\n\xff\xfe\n", &HashSet::new()).valid);
    }
}
