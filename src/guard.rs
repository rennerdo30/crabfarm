//! Executor-side validation of a `JobSpec` (CF-8, DESIGN section 9).
//!
//! The submitter classifies and maps the command line, but the executing peer
//! must not trust that: a peer that receives `Arg::Plain("--out-dir=D:\x")`
//! would pass the path through unmapped. Everything here mirrors
//! `rustc_args::classify`: the argument list is checked with the same flag
//! parser, and anything the classifier would have refused, or would have
//! turned into a structured path, is refused when it shows up in the wrong
//! shape. A refused job ends in `Message::Error`, and the submitter runs it
//! locally (DESIGN section 10).

use crate::paths::valid_component;
use crate::proto::{Arg, ClientPath, EnvValue, JobSpec, PathStyle};
use crate::rustc_args::{ALLOWED_EMIT, SEARCH_KINDS, looks_absolute, parse_flag};

/// Per-job caps, checked before anything is written (CF-8).
pub const MAX_INPUT_FILES: usize = 100_000;
pub const MAX_DIRS: usize = 10_000;
pub const MAX_MATERIALIZED_BYTES: u64 = 512 * 1024 * 1024;

/// Stands in for a structured (mapped) path while the argument list is
/// checked as text. Plain arguments containing it are refused first, so it
/// cannot be forged.
const MARK: char = '\u{1}';

/// Unicode combining marks (the blocks canonical decompositions use). Not a
/// complete table on purpose: see [`fold_component`].
fn is_combining(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x0610..=0x061A
        | 0x064B..=0x065F | 0x0900..=0x0903 | 0x093A..=0x094F | 0x1AB0..=0x1AFF
        | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0x3099..=0x309A | 0xFE20..=0xFE2F)
}

/// What a path component is called on a file system that ignores case and
/// Unicode normalization (Windows, default macOS): two names that can be the
/// same file there have the same result.
///
/// Case is folded with `to_lowercase`. Normalization (NFC/NFD) cannot be
/// computed without the Unicode tables, and no crate that has them is in the
/// tree, so the rule is conservative instead (fail closed): a name with any
/// non-ASCII character or combining mark is reduced to its ASCII skeleton,
/// that is the name without those characters and without an ASCII letter that a
/// combining mark follows. A composed `\u{e9}` and a decomposed `e\u{301}` both
/// reduce to the same thing, as does every other pair of canonically equivalent
/// names; names that merely look alike after that (`\u{e9}.rs` and `\u{e8}.rs`)
/// are reported as colliding too. The price is a refused job (it runs on the
/// submitter) in a rare case, never a silent overwrite. Known gap: the two
/// ASCII punctuation characters that have canonical equivalents outside
/// ASCII (`;` is equivalent to U+037E, `` ` `` to U+1FEF).
fn fold_component(c: &str) -> String {
    let lower = c.to_lowercase();
    if lower.is_ascii() {
        return lower;
    }
    let mut skeleton = String::new();
    for ch in lower.chars() {
        if is_combining(ch) {
            if skeleton.ends_with(|b: char| b.is_ascii_alphabetic()) {
                skeleton.pop();
            }
        } else if ch.is_ascii() {
            skeleton.push(ch);
        }
    }
    // The marker keeps a skeleton apart from an ASCII name with the same text.
    format!("\u{1}{skeleton}")
}

fn collision_key(p: &ClientPath, case_insensitive: bool) -> String {
    let parts = p.path.split(['/', '\\']).filter(|c| !c.is_empty());
    let mut key = format!("{:?}", p.style);
    for c in parts {
        key.push('\0');
        if case_insensitive {
            key.push_str(&fold_component(c));
        } else {
            key.push_str(c);
        }
    }
    key
}

/// Refuse a job in which two inputs with different content would land on the
/// same file in the sandbox (CF-19): the second write would silently replace
/// the first. On a case-insensitive sandbox file system, names that differ only
/// in case or in Unicode normalization count as the same file; two inputs with
/// the same content under such names are harmless and accepted.
pub fn check_collisions(spec: &JobSpec, case_insensitive: bool) -> Result<(), String> {
    let mut seen: std::collections::HashMap<String, &crate::proto::BlobHash> =
        std::collections::HashMap::with_capacity(spec.inputs.len());
    for i in &spec.inputs {
        if let Some(h) = seen.insert(collision_key(&i.path, case_insensitive), &i.hash)
            && *h != i.hash
        {
            return Err("two inputs with different content map to the same file".into());
        }
    }
    Ok(())
}

/// Caps on the declared inputs and directories.
pub fn check_caps(spec: &JobSpec) -> Result<(), String> {
    if spec.inputs.len() > MAX_INPUT_FILES {
        return Err(format!(
            "job has {} input files (limit {MAX_INPUT_FILES})",
            spec.inputs.len()
        ));
    }
    if spec.dirs.len() > MAX_DIRS {
        return Err(format!(
            "job has {} directories (limit {MAX_DIRS})",
            spec.dirs.len()
        ));
    }
    let total = spec
        .inputs
        .iter()
        .fold(0u64, |a, i| a.saturating_add(i.size));
    if total > MAX_MATERIALIZED_BYTES {
        return Err(format!(
            "job inputs are {total} bytes (limit {MAX_MATERIALIZED_BYTES})"
        ));
    }
    Ok(())
}

/// A relative path value that stays below the working directory: no root, no
/// drive, no `..`, every component a plain name.
fn safe_relative(s: &str) -> bool {
    if s.is_empty() || looks_absolute(PathStyle::Windows, s) || looks_absolute(PathStyle::Posix, s)
    {
        return false;
    }
    s.split(['/', '\\'])
        .all(|c| c.is_empty() || c == "." || valid_component(c))
}

/// A path-carrying value: the structured marker alone, or a safe relative path.
fn path_value(v: &str) -> bool {
    v == MARK.to_string() || safe_relative(v)
}

/// Longest crate name accepted (it ends up in file names and log lines).
const MAX_CRATE_NAME: usize = 128;

/// rustc's crate-name syntax: letters, digits and `_` (Unicode letters allowed).
fn crate_name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.chars().count() <= MAX_CRATE_NAME
        && n.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// The value of `--crate-name` in a job's arguments, in either spelling.
fn args_crate_name(args: &[Arg]) -> Option<&str> {
    let mut found = None;
    let mut i = 0;
    while i < args.len() {
        if let Arg::Plain(a) = &args[i] {
            if a == "--crate-name" {
                if let Some(Arg::Plain(v)) = args.get(i + 1) {
                    found = Some(v.as_str());
                }
                i += 1;
            } else if let Some(v) = a.strip_prefix("--crate-name=") {
                found = Some(v);
            }
        }
        i += 1;
    }
    found
}

/// `JobSpec.crate_name` ends up in logs and status output on this peer, so it
/// has to be what it says it is: a valid crate name that is the one rustc is
/// told to compile (CF-19).
fn check_crate_name(spec: &JobSpec) -> Result<(), String> {
    if !crate_name_ok(&spec.crate_name) {
        return Err("job has an invalid crate name".into());
    }
    if args_crate_name(&spec.args) != Some(spec.crate_name.as_str()) {
        return Err("job crate name differs from --crate-name".into());
    }
    Ok(())
}

/// `-C key[=value]` as the classifier treats it (CF-19). The keys are an
/// allow-list (`rustc_args::CODEGEN_REMOTE_OK`); a key that is not on it makes
/// the job local, so a new or dangerous rustc option never runs on a peer by
/// accident (`instrument-coverage`, `profile-*`, `save-temps`, `ar`, `dlltool`,
/// `llvm-args`, `remark`, ... are all outside). `link-arg`/`link-args` are
/// collected into `link_args` and judged once the crate type is known.
fn codegen_ok(value: &str, link_args: &mut Vec<String>) -> Result<(), String> {
    if value.contains(MARK) && value != format!("linker={MARK}") {
        return Err("-C takes a mapped path".into());
    }
    let (key, val) = value.split_once('=').unwrap_or((value, ""));
    let abs =
        |v: &str| looks_absolute(PathStyle::Windows, v) || looks_absolute(PathStyle::Posix, v);
    match key {
        "incremental" => Err("-C incremental".into()),
        "target-cpu" if val == "native" => Err("-C target-cpu=native".into()),
        // Only `linker` is supported among the programs rustc would start: as a
        // bare name or as a structured path the executor resolves itself.
        "linker" => {
            if val == MARK.to_string() || (!val.is_empty() && !val.contains(['/', '\\', MARK])) {
                Ok(())
            } else {
                Err("-C linker is not a bare name or a mapped path".into())
            }
        }
        "extra-filename" | "metadata" => {
            if val.contains(['/', '\\', ':', MARK]) || val.contains("..") {
                Err(format!("-C {key} holds a path"))
            } else {
                Ok(())
            }
        }
        "link-arg" | "link-args" => {
            if val.contains(MARK) || val.starts_with('@') {
                Err(format!("-C {key}"))
            } else {
                link_args.push(val.to_string());
                Ok(())
            }
        }
        _ if !crate::rustc_args::codegen_key_remote_ok(key) => {
            Err(format!("-C {key} is not allowed on a peer"))
        }
        _ if val.contains(MARK) || abs(val) || val.starts_with('@') => {
            Err(format!("-C {key} takes a path"))
        }
        _ => Ok(()),
    }
}

/// `-C link-arg` values of a job, judged by what the crate links. A library
/// (`lib`, `rlib`) is not linked, so rustc never hands the arguments to a
/// linker. A proc macro is a dynamic library that IS linked, here: allowed only
/// from the same host as the submitter (the arguments are meant for that
/// linker) and with no path separator anywhere in them, so nothing can name a
/// file of the submitter's machine or of this one.
fn link_args_ok(link_args: &[String], proc_macro: bool, same_host: bool) -> Result<(), String> {
    if !proc_macro || link_args.is_empty() {
        return Ok(());
    }
    if !same_host {
        return Err("-C link-arg for a proc macro on a different host".into());
    }
    if link_args.iter().any(|a| a.contains(['/', '\\'])) {
        return Err("-C link-arg with a path for a proc macro".into());
    }
    Ok(())
}

/// Check the whole argument list. `Ok` means every path-carrying flag holds a
/// structured path (mapped on this peer) or a safe relative path.
///
/// `same_host` is true when this peer's toolchain has the host triple of the
/// submitter's (see `link_args_ok`).
pub fn validate_args(args: &[Arg], same_host: bool) -> Result<(), String> {
    let mut v: Vec<String> = Vec::with_capacity(args.len());
    let mut structured = Vec::with_capacity(args.len());
    for a in args {
        let (text, is_path) = match a {
            Arg::Plain(s) => (s.clone(), false),
            Arg::Path(_) => (MARK.to_string(), true),
            Arg::PrefixedPath { prefix, .. } => (format!("{prefix}{MARK}"), true),
            Arg::RemapFrom { to, .. } => (format!("{MARK}={to}"), true),
        };
        let forged = match a {
            Arg::Plain(s) => s.contains(MARK),
            Arg::PrefixedPath { prefix, .. } => prefix.contains(MARK),
            Arg::RemapFrom { to, .. } => to.contains(MARK),
            Arg::Path(_) => false,
        };
        if forged {
            return Err("argument contains a reserved character".into());
        }
        v.push(text);
        structured.push(is_path);
    }

    let mut input = false;
    let mut crate_types: Vec<String> = Vec::new();
    let mut link_args: Vec<String> = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let a = v[i].as_str();
        match a {
            "-vV" | "-V" | "--version" | "-h" | "--help" | "--test" | "-" => {
                return Err(format!("flag {a}"));
            }
            "-g" | "-O" | "-v" | "--verbose" => {
                i += 1;
                continue;
            }
            _ => {}
        }
        if a.starts_with('@') {
            return Err("@argfile".into());
        }
        let tok = parse_flag(&v, i)?;
        if let Some(tok) = tok {
            // A structured argument may only be the value of a flag that maps paths.
            let val = tok.value.as_str();
            match tok.name {
                "--print" | "--explain" | "--sysroot" | "-Z" => {
                    return Err(format!("flag {}", tok.name));
                }
                "--crate-type" => {
                    if val.contains(MARK) {
                        return Err("--crate-type".into());
                    }
                    for t in val.split(',').map(str::trim).filter(|t| !t.is_empty()) {
                        if !matches!(t, "lib" | "rlib" | "proc-macro") {
                            return Err(format!("crate type {t}"));
                        }
                        crate_types.push(t.to_string());
                    }
                }
                "--crate-name" => {
                    if !crate_name_ok(val) {
                        return Err("--crate-name".into());
                    }
                }
                "--target" => {
                    if val.is_empty() || val.ends_with(".json") || val.contains(['/', '\\', MARK]) {
                        return Err("--target".into());
                    }
                }
                "--out-dir" | "-o" => {
                    if !path_value(val) || val == "-" {
                        return Err(format!("{} is not a mapped path", tok.name));
                    }
                }
                "--extern" => match val.split_once('=') {
                    None => {
                        if val.contains(['/', '\\', MARK]) {
                            return Err("--extern".into());
                        }
                    }
                    Some((left, path)) => {
                        if left.contains(['/', '\\', MARK]) || !path_value(path) {
                            return Err("--extern path is not mapped".into());
                        }
                    }
                },
                "-L" => {
                    let path = match val.split_once('=') {
                        Some((k, p)) if SEARCH_KINDS.contains(&k) => p,
                        _ => val,
                    };
                    if !path_value(path) {
                        return Err("-L path is not mapped".into());
                    }
                }
                "-l" => {
                    if val.contains(['/', '\\', MARK]) {
                        return Err("-l".into());
                    }
                }
                "--emit" => {
                    for it in val.split(',').filter(|s| !s.is_empty()) {
                        let (kind, path) =
                            it.split_once('=').map_or((it, None), |(k, p)| (k, Some(p)));
                        if !ALLOWED_EMIT.contains(&kind) {
                            return Err(format!("--emit {kind}"));
                        }
                        if path.is_some_and(|p| !path_value(p)) {
                            return Err("--emit path is not mapped".into());
                        }
                    }
                }
                "--remap-path-prefix" => {
                    // FROM is mapped (marker) or free text; TO is free text. Nothing is opened.
                    if val.contains(MARK) && !val.starts_with(&format!("{MARK}=")) {
                        return Err("--remap-path-prefix".into());
                    }
                }
                "-C" => codegen_ok(val, &mut link_args)?,
                _ => {
                    if val.contains(MARK) {
                        return Err(format!("{} takes no path", tok.name));
                    }
                }
            }
            // Each token of the flag must agree with `structured`: a structured
            // argument can only sit in a value position, which the checks above
            // validated through the marker.
            i += tok.original.len();
            continue;
        }
        // Unknown flag: harmless only without a path attached or following.
        if a.starts_with('-') {
            if a.contains(MARK) {
                return Err(format!("unknown flag {a} with a mapped path"));
            }
            if let Some((_, val)) = a.split_once('=')
                && (looks_absolute(PathStyle::Windows, val)
                    || looks_absolute(PathStyle::Posix, val))
            {
                return Err(format!("unknown flag {a} with a path value"));
            }
            if let Some(next) = v.get(i + 1)
                && !next.starts_with('-')
                && (structured[i + 1]
                    || looks_absolute(PathStyle::Windows, next)
                    || looks_absolute(PathStyle::Posix, next))
            {
                return Err(format!("unknown flag {a} followed by a path"));
            }
            i += 1;
            continue;
        }
        // Positional: the crate root, once.
        if input {
            return Err("more than one input".into());
        }
        if !path_value(a) {
            return Err("input is not a mapped path".into());
        }
        input = true;
        i += 1;
    }
    if !input {
        return Err("no input file".into());
    }
    if crate_types.is_empty() {
        return Err("no --crate-type".into());
    }
    link_args_ok(
        &link_args,
        crate_types.iter().any(|t| t == "proc-macro"),
        same_host,
    )?;
    Ok(())
}

/// Names `CARGO_PKG_<X>` that cargo sets for every crate it compiles.
const PKG_SUFFIXES: [&str; 14] = [
    "VERSION",
    "VERSION_MAJOR",
    "VERSION_MINOR",
    "VERSION_PATCH",
    "VERSION_PRE",
    "AUTHORS",
    "NAME",
    "DESCRIPTION",
    "HOMEPAGE",
    "REPOSITORY",
    "LICENSE",
    "LICENSE_FILE",
    "RUST_VERSION",
    "README",
];

/// The variables a job gets from the submitter without anybody listing them
/// (CF-18, DESIGN section 9). Allow-list, matched case-insensitively (Windows
/// environment names are):
///
/// - what cargo sets for the crates it compiles ("Environment variables Cargo
///   sets for crates", doc.rust-lang.org/cargo/reference/environment-variables.html):
///   `CARGO`, `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH`, `CARGO_MANIFEST_LINKS`,
///   `CARGO_PKG_*` (the fixed set above, not a prefix), `CARGO_CRATE_NAME`,
///   `CARGO_BIN_NAME`, `CARGO_PRIMARY_PACKAGE`, `CARGO_TARGET_TMPDIR`,
///   `CARGO_RUSTC_CURRENT_DIR`, `CARGO_BIN_EXE_<name>`, `OUT_DIR`;
/// - `CARGO_CFG_*`: the cfg values cargo derives from the target; cargo sets
///   them for build scripts only, but they carry no user data and are accepted
///   for a crate whose build script re-exports them with `rustc-env`;
/// - `RUSTC_BOOTSTRAP`: not set by cargo, but the user's own value changes what
///   rustc accepts, so a build that has it set must get it on the peer too.
///
/// Everything else is not forwarded unless the user lists it in `forward_env`
/// (and the peer in `accept_env`): `DEP_*`, `TARGET`, `HOST`, `PROFILE`,
/// `NUM_JOBS`, `OPT_LEVEL` and `DEBUG` are build-script variables that cargo
/// does not pass to rustc, and nothing else with a `CARGO_` prefix goes along:
/// `CARGO_REGISTRY_TOKEN` and `CARGO_REGISTRIES_<name>_TOKEN` are credentials.
/// `CARGO_MAKEFLAGS` is machine-local (jobserver handles) and never sent.
pub fn design_env(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    if matches!(
        up.as_str(),
        "CARGO"
            | "CARGO_MANIFEST_DIR"
            | "CARGO_MANIFEST_PATH"
            | "CARGO_MANIFEST_LINKS"
            | "CARGO_CRATE_NAME"
            | "CARGO_BIN_NAME"
            | "CARGO_PRIMARY_PACKAGE"
            | "CARGO_TARGET_TMPDIR"
            | "CARGO_RUSTC_CURRENT_DIR"
            | "OUT_DIR"
            | "RUSTC_BOOTSTRAP"
    ) {
        return true;
    }
    if let Some(rest) = up.strip_prefix("CARGO_PKG_") {
        return PKG_SUFFIXES.contains(&rest);
    }
    up.strip_prefix("CARGO_CFG_")
        .or_else(|| up.strip_prefix("CARGO_BIN_EXE_"))
        .is_some_and(|rest| !rest.is_empty())
}

/// Whether two names are the same environment variable on the machine this
/// code runs on (CF-25): names are case-insensitive on Windows only. On Unix
/// `Path` and `PATH` are two variables, so listing one in `forward_env` or
/// `accept_env` must not select the other.
pub fn env_name_eq(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// True when `name` is a variable the farm must never accept, even when the
/// submitter lists it: it changes which program runs or how it is loaded, or
/// belongs to the machine (search path, loader, wrappers, temp and home dirs).
fn never_forward(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    up.is_empty()
        || up.contains(['=', '\0'])
        || up.starts_with("LD_")
        || up.starts_with("DYLD_")
        || up.starts_with("RUSTC")
        || up.starts_with("RUSTDOC")
        || up.starts_with("RUSTUP")
        || up.starts_with("CARGO_MAKEFLAGS")
        || up.starts_with("CARGO_REGISTR")
        // Tools the compiler or linker starts, or whose search path and
        // network behaviour they change (CF-25). `RUST_LOG` is not here: it only
        // makes rustc print more, which the submitter treats as diagnostics.
        || up.starts_with("SSL_")
        || up.starts_with("GIT_")
        || up.starts_with("SSH_")
        || up.starts_with("PYTHON")
        || up.starts_with("PERL5")
        || up.starts_with("RUBY")
        || matches!(
            up.as_str(),
            "NODE_OPTIONS"
                | "NODE_PATH"
                | "_LINK_"
                | "LINK"
                | "CL"
                | "_CL_"
                | "COMPILER_PATH"
                | "GCC_EXEC_PREFIX"
                | "LIBRARY_PATH"
                | "CPATH"
                | "C_INCLUDE_PATH"
                | "CPLUS_INCLUDE_PATH"
                | "SDKROOT"
                | "DEVELOPER_DIR"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "ALL_PROXY"
                | "NO_PROXY"
                | "JAVA_TOOL_OPTIONS"
        )
        || matches!(
            up.as_str(),
            "PATH"
                | "PATHEXT"
                | "CARGO_HOME"
                | "RUSTFLAGS"
                | "MAKEFLAGS"
                | "MFLAGS"
                | "MAKELEVEL"
                | "SYSTEMROOT"
                | "WINDIR"
                | "COMSPEC"
                | "TEMP"
                | "TMP"
                | "TMPDIR"
                | "HOME"
                | "USERPROFILE"
                | "APPDATA"
                | "LOCALAPPDATA"
                | "LIB"
                | "INCLUDE"
                | "LIBPATH"
                | "SHELL"
                | "IFS"
        )
}

/// Names a submitter may put into `forward_env` at all. Also used on the
/// submitter side, so `forward_env = ["PATH"]` is never sent. This is a floor
/// under the executor's own allow-list (`Policy::accept_env`), not a way to
/// accept anything it does not exclude.
pub fn extra_env_name_ok(name: &str) -> bool {
    !never_forward(name)
}

/// Most names a job may declare in `build_env` (CF-21), on both sides.
pub const MAX_BUILD_ENV: usize = 64;
/// Longest `build_env` name.
pub const MAX_BUILD_ENV_NAME: usize = 128;

/// A name the wrapper may declare in `build_env` and the executor accepts:
/// environment-variable syntax (`[A-Za-z_][A-Za-z0-9_]*`, at most
/// [`MAX_BUILD_ENV_NAME`] bytes) and not on the never-list. The same function
/// runs on both sides, so what the wrapper sends is what the executor accepts.
pub fn build_env_name_ok(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.len() <= MAX_BUILD_ENV_NAME
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !never_forward(name)
}

/// What the executing node decides on its own, never what the job says.
pub struct Policy<'a> {
    /// Host triple of the toolchain that would run the job.
    pub peer_host: &'a str,
    /// Names the operator of this node accepts as `extra_env` (config
    /// `accept_env`). Empty by default: a job that declares any other
    /// variable is refused and runs on the submitter.
    pub accept_env: &'a [String],
    /// Whether this node's own environment defines a name.
    pub node_has: &'a dyn Fn(&str) -> bool,
    /// The sandbox file system ignores case and Unicode normalization.
    pub case_insensitive: bool,
    /// Config `accept_build_env` (CF-25): take the names a submitter's build
    /// script declared. A job that declares any is refused when this is false.
    pub accept_build_env: bool,
}

/// Check the job environment (CF-8, CF-18, CF-19, CF-21).
///
/// Every variable must be on the allow-list (`design_env`), be declared in
/// `build_env` (names the crate's own build script emitted; accepted without
/// operator listing, subject to the never-list), or be declared in
/// `extra_env` AND listed in the operator's `accept_env`; the hard floor
/// (`never_forward`: `PATH`, loader, wrapper and machine variables) holds even
/// then, and a declared name may not override a variable of the peer's own
/// environment.
///
/// `RUSTC_BOOTSTRAP` is accepted because DESIGN section 9 forwards it and it
/// only reaches the job when the submitter set it itself (the job environment
/// is the submitter's forwarded set); a local build with that variable behaves
/// the same. The peer's own `RUSTC_BOOTSTRAP` never reaches a job
/// (`exec::is_build_var`).
pub fn validate_env(spec: &JobSpec, policy: &Policy) -> Result<(), String> {
    for n in &spec.extra_env {
        let accepted = policy.accept_env.iter().any(|a| env_name_eq(a, n));
        if !extra_env_name_ok(n) || design_env(n) || !accepted || (policy.node_has)(n) {
            return Err("job declares a variable this node does not accept".into());
        }
    }
    if !spec.build_env.is_empty() && !policy.accept_build_env {
        return Err("this node does not accept build-script variables".into());
    }
    if spec.build_env.len() > MAX_BUILD_ENV {
        return Err("job declares too many build-script variables".into());
    }
    for n in &spec.build_env {
        // Same rule as for `extra_env`: a declared name never overrides what
        // the peer itself has set (the job then runs on the submitter).
        if !build_env_name_ok(n) || (policy.node_has)(n) {
            return Err("job declares a build-script variable this node does not accept".into());
        }
    }
    for (k, v) in &spec.env {
        if never_forward(k) && !env_name_eq(k, "RUSTC_BOOTSTRAP") {
            return Err("job sets a variable the farm does not forward".into());
        }
        let declared = spec.extra_env.iter().any(|e| env_name_eq(e, k))
            || spec.build_env.iter().any(|e| env_name_eq(e, k));
        if !design_env(k) && !declared {
            return Err("job sets a variable that is not forwarded".into());
        }
        if let EnvValue::Plain(s) = v
            && s.contains('\0')
        {
            return Err("variable value contains NUL".into());
        }
    }
    Ok(())
}

/// Everything that can be decided without touching the file system.
pub fn validate_spec(spec: &JobSpec, policy: &Policy) -> Result<(), String> {
    check_caps(spec)?;
    check_crate_name(spec)?;
    check_collisions(spec, policy.case_insensitive)?;
    validate_args(&spec.args, spec.toolchain.host == policy.peer_host)?;
    validate_env(spec, policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{ClientPath, InputFile, JobId, ToolchainId};

    /// `validate_args` for a job from the submitter's own host.
    fn va(a: &[Arg]) -> Result<(), String> {
        validate_args(a, true)
    }

    /// `validate_env` for a node that accepts `accept` and has no own variables.
    fn ve(spec: &JobSpec, accept: &[&str]) -> Result<(), String> {
        let none = |_: &str| false;
        let accept: Vec<String> = accept.iter().map(|s| s.to_string()).collect();
        validate_env(
            spec,
            &Policy {
                peer_host: "h",
                accept_env: &accept,
                node_has: &none,
                case_insensitive: false,
                accept_build_env: true,
            },
        )
    }

    fn cp(s: &str) -> ClientPath {
        ClientPath::parse(PathStyle::Windows, s).expect("test path")
    }
    fn p(s: &str) -> Arg {
        Arg::Plain(s.to_string())
    }
    /// What the classifier produces for a typical cargo lib build.
    fn good() -> Vec<Arg> {
        vec![
            p("--crate-name"),
            p("x"),
            p("--edition=2021"),
            Arg::Path(cp(r"C:\w\src\lib.rs")),
            p("--crate-type"),
            p("lib"),
            p("--emit=dep-info,metadata,link"),
            p("--error-format=json"),
            p("--json=artifacts,diagnostic-rendered-ansi"),
            p("--out-dir"),
            Arg::Path(cp(r"C:\w\target\deps")),
            p("-C"),
            p("embed-bitcode=no"),
            p("-C"),
            p("extra-filename=-abc"),
            p("-L"),
            Arg::PrefixedPath {
                prefix: "dependency=".into(),
                path: cp(r"C:\w\target\deps"),
            },
            p("--extern"),
            Arg::PrefixedPath {
                prefix: "dep=".into(),
                path: cp(r"C:\w\target\deps\libdep-1.rlib"),
            },
            p("--remap-path-prefix"),
            Arg::RemapFrom {
                from: cp(r"C:\w"),
                to: "/rustc".into(),
            },
            p("-l"),
            p("static=foo"),
            p("--cfg"),
            p("feature=\"std\""),
            p("--target"),
            p("x86_64-pc-windows-msvc"),
        ]
    }

    #[test]
    fn classifier_shaped_arguments_pass() {
        assert_eq!(va(&good()), Ok(()));
        // Attached forms and a mapped linker.
        let mut a = good();
        a.push(Arg::PrefixedPath {
            prefix: "-Clinker=".into(),
            path: cp(r"C:\tools\link.exe"),
        });
        a.push(p("--emit=metadata=target/x.rmeta"));
        a.push(p("-Ldependency=deps"));
        assert_eq!(va(&a), Ok(()));
    }

    #[test]
    fn plain_path_carrying_flags_are_refused() {
        let cases: Vec<Vec<Arg>> = vec![
            vec![p("--out-dir=D:\\x")],
            vec![p("--out-dir"), p("D:\\x")],
            vec![p("-o"), p("D:\\x\\y.rlib")],
            vec![p("-oD:\\x")],
            vec![p("--emit=link=D:\\x\\y")],
            vec![p("--emit"), p("link=/etc/x")],
            vec![p("-C"), p("linker=D:\\tools\\evil.exe")],
            vec![p("-Clinker=/usr/bin/evil")],
            vec![p("-C"), p("linker=..\\evil.exe")],
            vec![p("@D:\\x\\args.txt")],
            vec![p("-Zunstable-options")],
            vec![p("-Z"), p("build-std")],
            vec![p("--sysroot"), p("D:\\x")],
            vec![p("--sysroot=D:\\x")],
            vec![p("-C"), p("incremental=D:\\x")],
            vec![p("-C"), p("incremental")],
            vec![p("--print"), p("sysroot")],
            vec![p("--print=file-names")],
            vec![p("-L"), p("D:\\x")],
            vec![p("-LD:\\x")],
            vec![p("-L"), p("native=D:\\x")],
            vec![p("--extern"), p("a=D:\\x\\liba.rlib")],
            vec![p("--extern"), p("a=..\\..\\liba.rlib")],
            vec![p("-C"), p("target-cpu=native")],
            vec![p("-C"), p("ar=evil")],
            vec![p("-C"), p("save-temps=D:\\x")],
            vec![p("--crate-type"), p("bin")],
            vec![p("--emit=asm")],
            vec![p("--bogus=D:\\x")],
            vec![p("--bogus"), p("/abs")],
            vec![p("--cfg"), Arg::Path(cp(r"C:\a"))],
            vec![p("-C"), Arg::Path(cp(r"C:\a"))],
            vec![p("--unknown"), Arg::Path(cp(r"C:\a"))],
            vec![p("--target"), p("D:\\x\\t.json")],
        ];
        for extra in cases {
            let mut a = good();
            a.extend(extra.clone());
            assert!(va(&a).is_err(), "must refuse {extra:?}");
        }
    }

    #[test]
    fn structure_is_required() {
        let mut a = good();
        a.push(Arg::Path(cp(r"C:\w\src\other.rs")));
        assert!(va(&a).is_err(), "two inputs");
        assert!(va(&[p("--crate-type"), p("lib")]).is_err(), "no input");
        let mut b = good();
        b.retain(|x| !matches!(x, Arg::Plain(s) if s == "lib"));
        b.retain(|x| !matches!(x, Arg::Plain(s) if s == "--crate-type"));
        assert!(va(&b).is_err(), "no crate type");
        // The marker cannot be forged.
        let mut c = good();
        c.push(p("--cfg"));
        c.push(p("x\u{1}"));
        assert!(va(&c).is_err());
    }

    fn spec(env: Vec<(&str, EnvValue)>, extra: &[&str]) -> JobSpec {
        JobSpec {
            job_id: JobId([1; 16]),
            toolchain: ToolchainId {
                release: "1".into(),
                commit_hash: "a".into(),
                commit_date: "d".into(),
                host: "h".into(),
            },
            target: "t".into(),
            args: good(),
            cwd: cp(r"C:\w"),
            env: env.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            extra_env: extra.iter().map(|s| s.to_string()).collect(),
            build_env: Vec::new(),
            inputs: Vec::new(),
            dirs: Vec::new(),
            out_dir: cp(r"C:\w\target\deps"),
            crate_name: "x".into(),
            no_retain: false,
            needs_host: false,
        }
    }

    fn plain(s: &str) -> EnvValue {
        EnvValue::Plain(s.into())
    }

    #[test]
    fn env_follows_design_section_9() {
        let ok = spec(
            vec![
                ("CARGO_PKG_NAME", plain("x")),
                ("OUT_DIR", EnvValue::Path(cp(r"C:\o"))),
                ("CARGO_CRATE_NAME", plain("x")),
                ("RUSTC_BOOTSTRAP", plain("1")),
                ("MY_VAR", plain("v")),
            ],
            &["MY_VAR"],
        );
        assert_eq!(ve(&ok, &["MY_VAR"]), Ok(()));
        // CF-18: credentials and settings of cargo are refused when
        // undeclared, whatever their prefix; so are build-script variables.
        for bad in [
            "CARGO_REGISTRY_TOKEN",
            "CARGO_REGISTRIES_PRIVATE_TOKEN",
            "CARGO_HOME",
            "CARGO_PKG_SECRET",
            "GITHUB_TOKEN",
            "DEP_A_B",
            "TARGET",
            "HOST",
            "PROFILE",
            "NUM_JOBS",
            "OPT_LEVEL",
            "DEBUG",
        ] {
            let s = spec(vec![(bad, plain("x"))], &[]);
            assert!(ve(&s, &[bad]).is_err(), "{bad} must be refused");
        }
        for bad in [
            "PATH",
            "Path",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "RUSTFLAGS",
            "SystemRoot",
            "CARGO_MAKEFLAGS",
            "RUSTUP_TOOLCHAIN",
            "HOME",
        ] {
            let s = spec(vec![(bad, plain("x"))], &[]);
            assert!(ve(&s, &[]).is_err(), "{bad} undeclared");
            // Declaring it does not help, and neither does accepting it.
            let s = spec(vec![(bad, plain("x"))], &[bad]);
            assert!(ve(&s, &[]).is_err(), "{bad} declared");
            assert!(ve(&s, &[bad]).is_err(), "{bad} declared and accepted");
        }
        // An undeclared variable is refused; a declared one that the node itself defines too is refused.
        assert!(ve(&spec(vec![("MY_VAR", plain("v"))], &[]), &["MY_VAR"]).is_err());
        let none_accepted: Vec<String> = vec!["MY_VAR".into()];
        let has = |n: &str| n == "MY_VAR";
        assert!(
            validate_env(
                &spec(vec![("MY_VAR", plain("v"))], &["MY_VAR"]),
                &Policy {
                    peer_host: "h",
                    accept_env: &none_accepted,
                    node_has: &has,
                    case_insensitive: false,
                    accept_build_env: true,
                }
            )
            .is_err()
        );
    }

    /// CF-21: names the crate's build script emitted travel in `build_env` and
    /// need no `accept_env`; the never-list and the cap still hold, and nothing
    /// outside allow-list + declared names gets through.
    #[test]
    fn build_env_names_are_accepted_by_source_not_by_operator_list() {
        let mut ok = spec(
            vec![("CF_BUILT", plain("yes")), ("CARGO_PKG_NAME", plain("x"))],
            &[],
        );
        // Not declared anywhere: refused.
        assert!(ve(&ok, &[]).is_err());
        ok.build_env = vec!["CF_BUILT".into()];
        assert_eq!(ve(&ok, &[]), Ok(()));
        // A name in env that is not declared stays refused next to a declared one.
        ok.env.push(("OTHER".into(), plain("v")));
        assert!(ve(&ok, &[]).is_err());
        // The never-list holds even when declared.
        for bad in [
            "PATH",
            "Path",
            "RUSTC_WRAPPER",
            "RUSTDOCFLAGS",
            "CARGO_HOME",
            "CARGO_REGISTRY_TOKEN",
            "CARGO_REGISTRIES_X_TOKEN",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "MAKEFLAGS",
            "CARGO_MAKEFLAGS",
        ] {
            let mut s = spec(vec![(bad, plain("x"))], &[]);
            s.build_env = vec![bad.into()];
            assert!(ve(&s, &[bad]).is_err(), "{bad} declared in build_env");
        }
        // Syntax and caps.
        for bad in ["", "1X", "A-B", "A=B", "A B", &"A".repeat(129)] {
            let mut s = spec(vec![], &[]);
            s.build_env = vec![bad.into()];
            assert!(ve(&s, &[]).is_err(), "{bad:?}");
        }
        let mut s = spec(vec![], &[]);
        s.build_env = (0..=MAX_BUILD_ENV).map(|i| format!("V{i}")).collect();
        assert!(ve(&s, &[]).is_err(), "count cap");
        s.build_env.truncate(MAX_BUILD_ENV);
        assert_eq!(ve(&s, &[]), Ok(()));
        // A declared name never overrides the peer's own variable.
        let mut s = spec(vec![("CF_BUILT", plain("yes"))], &[]);
        s.build_env = vec!["CF_BUILT".into()];
        let has = |n: &str| n == "CF_BUILT";
        assert!(
            validate_env(
                &s,
                &Policy {
                    peer_host: "h",
                    accept_env: &[],
                    node_has: &has,
                    case_insensitive: false,
                    accept_build_env: true,
                }
            )
            .is_err()
        );
    }

    /// CF-25: names that change how the compiler or linker runs, which tools it
    /// starts or where it goes on the network are never accepted, in any case and
    /// whether declared as `build_env` or `extra_env`.
    #[test]
    fn tool_affecting_names_are_on_the_never_list() {
        for bad in [
            "NODE_OPTIONS",
            "_LINK_",
            "LINK",
            "CL",
            "_CL_",
            "INCLUDE",
            "LIB",
            "LIBPATH",
            "COMPILER_PATH",
            "GCC_EXEC_PREFIX",
            "http_proxy",
            "HTTPS_PROXY",
            "All_Proxy",
            "no_proxy",
            "SSL_CERT_FILE",
            "GIT_DIR",
            "GIT_SSH_COMMAND",
            "SSH_AUTH_SOCK",
            "PYTHONPATH",
            "PERL5LIB",
            "RUBYOPT",
            "JAVA_TOOL_OPTIONS",
        ] {
            assert!(!build_env_name_ok(bad), "{bad} as build_env");
            assert!(!extra_env_name_ok(bad), "{bad} as forward_env");
            let mut s = spec(vec![(bad, plain("x"))], &[]);
            s.build_env = vec![bad.into()];
            assert!(ve(&s, &[bad]).is_err(), "{bad} accepted by the executor");
        }
        // Not tool-affecting, and the point of `build_env`: still accepted.
        // `RUST_LOG` only makes rustc talk more (its output is diagnostics).
        for ok in ["CF_BUILT", "BUILD_ID", "RUST_LOG", "VERGEN_SHA"] {
            assert!(build_env_name_ok(ok), "{ok}");
        }
    }

    /// CF-25: names compare case-insensitively on Windows only; on Unix a
    /// variable listed as `MY_VAR` is not `my_var`.
    #[test]
    fn name_matching_is_case_sensitive_off_windows() {
        assert!(env_name_eq("MY_VAR", "MY_VAR"));
        assert_eq!(env_name_eq("My_Var", "MY_VAR"), cfg!(windows));
        let job = spec(vec![("my_var", plain("v"))], &["my_var"]);
        assert_eq!(ve(&job, &["MY_VAR"]).is_ok(), cfg!(windows));
        assert_eq!(ve(&job, &["my_var"]), Ok(()));
        // An undeclared lower-case twin of a declared name is not declared.
        let mut twin = spec(vec![("My_Var", plain("v"))], &[]);
        twin.build_env = vec!["MY_VAR".into()];
        assert_eq!(ve(&twin, &[]).is_ok(), cfg!(windows));
    }

    /// CF-25: a node whose operator turned `accept_build_env` off refuses any
    /// job that declares build-script variables, and only those.
    #[test]
    fn accept_build_env_false_refuses_declared_build_variables() {
        let none = |_: &str| false;
        let refuse = Policy {
            peer_host: "h",
            accept_env: &[],
            node_has: &none,
            case_insensitive: false,
            accept_build_env: false,
        };
        let mut s = spec(vec![("CF_BUILT", plain("yes"))], &[]);
        s.build_env = vec!["CF_BUILT".into()];
        assert!(validate_env(&s, &refuse).is_err());
        let plain_job = spec(vec![("CARGO_PKG_NAME", plain("x"))], &[]);
        assert_eq!(validate_env(&plain_job, &refuse), Ok(()));
        let accept = Policy {
            accept_build_env: true,
            ..refuse
        };
        assert_eq!(validate_env(&s, &accept), Ok(()));
    }

    /// CF-19: a declared name is accepted only when the node's operator listed
    /// it (`accept_env`); the default is to refuse every declaration.
    #[test]
    fn declared_names_need_the_operators_allow_list() {
        let job = spec(vec![("MY_VAR", plain("v"))], &["MY_VAR"]);
        assert!(ve(&job, &[]).is_err(), "nothing accepted by default");
        assert!(ve(&job, &["OTHER"]).is_err());
        assert_eq!(
            ve(&job, &["my_var"]),
            Ok(()),
            "names compare case-insensitively"
        );
        // A name that is only declared (not set in env) is refused too.
        let declared_only = spec(vec![], &["MY_VAR"]);
        assert!(ve(&declared_only, &[]).is_err());
    }

    /// `-C` keys cargo really passes (taken from `cargo build -v` of the test
    /// fixture and the usual RUSTFLAGS) stay accepted.
    #[test]
    fn real_cargo_codegen_flags_pass() {
        for c in [
            "embed-bitcode=no",
            "debug-assertions=off",
            "debuginfo=2",
            "opt-level=3",
            "opt-level=s",
            "overflow-checks=on",
            "codegen-units=16",
            "panic=abort",
            "strip=debuginfo",
            "lto=thin",
            "split-debuginfo=packed",
            "metadata=a293107b07e6888e",
            "extra-filename=-4467f462d3a319e5",
            "prefer-dynamic",
            "target-feature=+crt-static",
            "target-cpu=x86-64-v3",
            "relocation-model=pic",
            "code-model=small",
            "force-frame-pointers=yes",
            "symbol-mangling-version=v0",
            "linker=link.exe",
            "linker-flavor=gcc",
            "link-arg=-fuse-ld=lld",
        ] {
            let mut a = good();
            a.push(p("-C"));
            a.push(p(c));
            assert_eq!(va(&a), Ok(()), "-C {c}");
        }
    }

    /// CF-19: any `-C` key outside the allow-list is refused, including the
    /// ones that write files or start programs.
    #[test]
    fn unknown_codegen_keys_are_refused() {
        for c in [
            "instrument-coverage",
            "instrument-coverage=all",
            "llvm-args=-time-passes",
            "remark=all",
            "passes=x",
            "no-prepopulate-passes",
            "save-temps",
            "ar=x",
            "dlltool=x",
            "profile-generate",
            "profile-use=x",
            "link-self-contained=yes",
            "linker-plugin-lto",
            "pgo-gen=x",
            "some-future-option=1",
            "opt-level=/etc/x",
            "opt-level=@file",
        ] {
            let mut a = good();
            a.push(p("-C"));
            a.push(p(c));
            assert!(va(&a).is_err(), "-C {c} must be refused");
            let mut b = good();
            b.push(p(&format!("-C{c}")));
            assert!(va(&b).is_err(), "-C{c} must be refused");
        }
    }

    /// CF-19: link arguments reach a linker only for a proc macro; there they
    /// need the submitter's host and no path.
    #[test]
    fn link_args_are_judged_by_what_gets_linked() {
        let with = |crate_type: &str, arg: &str| {
            let mut a = good();
            a.push(p("--crate-type"));
            a.push(p(crate_type));
            a.push(p("-C"));
            a.push(p(arg));
            a
        };
        // A library is not linked: nothing to check.
        assert_eq!(va(&with("rlib", "link-arg=-Wl,-rpath,/opt/lib")), Ok(()));
        // A proc macro from the same host, without a path.
        assert_eq!(va(&with("proc-macro", "link-arg=-fuse-ld=lld")), Ok(()));
        assert_eq!(
            va(&with("proc-macro", "link-args=-Wl,--as-needed -pthread")),
            Ok(())
        );
        // With a path (either separator), whatever it names.
        for bad in [
            "link-arg=-Wl,-rpath,/opt/lib",
            "link-arg=-LC:\\libs",
            "link-arg=-L/usr/lib",
            "link-args=-fuse-ld=/usr/bin/evil",
            "link-arg=@rsp",
        ] {
            assert!(va(&with("proc-macro", bad)).is_err(), "{bad}");
        }
        // From a different host the arguments are not meant for this linker.
        let a = with("proc-macro", "link-arg=-fuse-ld=lld");
        assert!(validate_args(&a, false).is_err());
        assert_eq!(validate_args(&with("rlib", "link-arg=-x"), false), Ok(()));
    }

    fn input(path: &str, content: &[u8]) -> InputFile {
        InputFile {
            path: cp(path),
            hash: crate::proto::BlobHash::of(content),
            size: content.len() as u64,
            executable: false,
        }
    }

    /// CF-19: names that are the same file on a case-insensitive file system
    /// must not carry different content.
    #[test]
    fn inputs_that_collide_on_a_case_insensitive_sandbox_are_refused() {
        let with = |inputs: Vec<InputFile>| {
            let mut s = spec(vec![], &[]);
            s.inputs = inputs;
            s
        };
        let case = with(vec![
            input(r"C:\w\Src\a.rs", b"one"),
            input(r"C:\w\src\A.rs", b"two"),
        ]);
        assert!(check_collisions(&case, true).is_err(), "case");
        // A case-sensitive file system keeps them apart.
        assert_eq!(check_collisions(&case, false), Ok(()));
        // The same content under both spellings is one file: fine.
        let same = with(vec![
            input(r"C:\w\Src\a.rs", b"one"),
            input(r"C:\w\src\A.rs", b"one"),
        ]);
        assert_eq!(check_collisions(&same, true), Ok(()));
        // Non-ASCII case folding and Unicode normalization (composed and decomposed).
        let upper = with(vec![
            input("C:\\w\\\u{c4}.rs", b"one"),
            input("C:\\w\\\u{e4}.rs", b"two"),
        ]);
        assert!(check_collisions(&upper, true).is_err(), "non-ASCII case");
        let nfc = with(vec![
            input("C:\\w\\caf\u{e9}.rs", b"one"),
            input("C:\\w\\cafe\u{301}.rs", b"two"),
        ]);
        assert!(check_collisions(&nfc, true).is_err(), "NFC against NFD");
        assert_eq!(check_collisions(&nfc, false), Ok(()));
        // Fail closed: names that only look alike after the reduction are refused too.
        let alike = with(vec![
            input("C:\\w\\\u{e9}.rs", b"one"),
            input("C:\\w\\\u{e8}.rs", b"two"),
        ]);
        assert!(check_collisions(&alike, true).is_err());
        // Unrelated names, a plain non-ASCII path and one name twice with one content pass.
        let ok = with(vec![
            input("C:\\Users\\Ren\u{e9}e\\p\\a.rs", b"one"),
            input("C:\\Users\\Ren\u{e9}e\\p\\b.rs", b"two"),
            input("C:\\Users\\Renee\\p\\a.rs", b"three"),
            input(r"C:\w\x.rs", b"4"),
            input(r"C:\w\x.rs", b"4"),
        ]);
        assert_eq!(check_collisions(&ok, true), Ok(()));
        // The very same path with two contents is refused everywhere.
        let dup = with(vec![input(r"C:\w\x.rs", b"1"), input(r"C:\w\x.rs", b"2")]);
        assert!(check_collisions(&dup, false).is_err());
    }

    /// CF-19: the crate name of the spec is a valid name and the one in the arguments.
    #[test]
    fn the_crate_name_must_be_valid_and_match_the_arguments() {
        let mut s = spec(vec![], &[]);
        s.crate_name = "x".into();
        assert_eq!(check_crate_name(&s), Ok(()));
        for bad in [
            "",
            "a-b",
            "a b",
            "x\n[ERROR] forged",
            "x\u{1b}[31m",
            "../x",
            "x/y",
            &"n".repeat(MAX_CRATE_NAME + 1),
        ] {
            s.crate_name = bad.into();
            assert!(check_crate_name(&s).is_err(), "{bad:?}");
        }
        // A valid name that is not the one being compiled.
        s.crate_name = "other".into();
        assert!(check_crate_name(&s).is_err());
        // Both spellings of the flag count.
        s.args = vec![Arg::Plain("--crate-name=other".into())];
        assert_eq!(check_crate_name(&s), Ok(()));
        // Unicode letters are valid in a crate name.
        s.crate_name = "caf\u{e9}".into();
        s.args = vec![
            Arg::Plain("--crate-name".into()),
            Arg::Plain("caf\u{e9}".into()),
        ];
        assert_eq!(check_crate_name(&s), Ok(()));
    }

    #[test]
    fn caps_apply_before_anything_is_written() {
        let mut s = spec(vec![], &[]);
        let f = |i: usize, size: u64| InputFile {
            path: cp(&format!(r"C:\w\f{i}.rs")),
            hash: crate::proto::BlobHash([0; 32]),
            size,
            executable: false,
        };
        s.inputs = (0..MAX_INPUT_FILES + 1).map(|i| f(i, 1)).collect();
        assert!(check_caps(&s).is_err());
        s.inputs = vec![f(0, MAX_MATERIALIZED_BYTES), f(1, 1)];
        assert!(check_caps(&s).is_err());
        s.inputs = vec![f(0, MAX_MATERIALIZED_BYTES)];
        assert_eq!(check_caps(&s), Ok(()));
        s.dirs = (0..MAX_DIRS + 1)
            .map(|i| cp(&format!(r"C:\w\d{i}")))
            .collect();
        assert!(check_caps(&s).is_err());
    }
}
