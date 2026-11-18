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
use crate::proto::{Arg, EnvValue, JobSpec, PathStyle};
use crate::rustc_args::{ALLOWED_EMIT, SEARCH_KINDS, looks_absolute, parse_flag};

/// Per-job caps, checked before anything is written (CF-8).
pub const MAX_INPUT_FILES: usize = 100_000;
pub const MAX_DIRS: usize = 10_000;
pub const MAX_MATERIALIZED_BYTES: u64 = 512 * 1024 * 1024;

/// Stands in for a structured (mapped) path while the argument list is
/// checked as text. Plain arguments containing it are refused first, so it
/// cannot be forged.
const MARK: char = '\u{1}';

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

fn crate_name_ok(n: &str) -> bool {
    !n.is_empty() && n.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// `-C key[=value]` as the classifier treats it.
fn codegen_ok(value: &str) -> Result<(), String> {
    if value.contains(MARK) && value != format!("linker={MARK}") {
        return Err("-C takes a mapped path".into());
    }
    let (key, val) = value.split_once('=').unwrap_or((value, ""));
    let abs =
        |v: &str| looks_absolute(PathStyle::Windows, v) || looks_absolute(PathStyle::Posix, v);
    match key {
        "incremental" => Err("-C incremental".into()),
        "target-cpu" if val == "native" => Err("-C target-cpu=native".into()),
        "profile-generate" | "profile-use" => Err(format!("-C {key}")),
        // Programs rustc would start: only `linker` is supported, as a bare
        // name or as a structured path the executor resolves itself.
        "ar" | "dlltool" | "linker-plugin-lto" | "save-temps" => Err(format!("-C {key}")),
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
        // Verbatim by contract (only matters when linking a proc macro).
        "link-arg" | "link-args" => {
            if val.contains(MARK) || val.starts_with('@') {
                Err(format!("-C {key}"))
            } else {
                Ok(())
            }
        }
        _ if val.contains(MARK) || abs(val) || val.starts_with('@') => {
            Err(format!("-C {key} takes a path"))
        }
        _ => Ok(()),
    }
}

/// Check the whole argument list. `Ok` means every path-carrying flag holds a
/// structured path (mapped on this peer) or a safe relative path.
pub fn validate_args(args: &[Arg]) -> Result<(), String> {
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
    let mut crate_types = 0usize;
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
                        crate_types += 1;
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
                "-C" => codegen_ok(val)?,
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
    if crate_types == 0 {
        return Err("no --crate-type".into());
    }
    Ok(())
}

/// Variables of the job environment the peer accepts from the submitter:
/// DESIGN section 9 (`wrapper::is_forwarded` without the config list), plus the
/// names the job declares in `extra_env` (the submitter's `forward_env` and
/// the names it learned per crate).
fn design_env(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    if up == "CARGO_MAKEFLAGS" {
        return false;
    }
    up == "CARGO"
        || up.starts_with("CARGO_")
        || up.starts_with("DEP_")
        || matches!(
            up.as_str(),
            "OUT_DIR"
                | "RUSTC_BOOTSTRAP"
                | "TARGET"
                | "HOST"
                | "PROFILE"
                | "NUM_JOBS"
                | "OPT_LEVEL"
                | "DEBUG"
        )
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
        || matches!(
            up.as_str(),
            "PATH"
                | "PATHEXT"
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

/// Names the submitter may declare beyond DESIGN section 9. Also used on the
/// submitter side, so a `forward_env` entry such as `PATH` is never sent.
pub fn extra_env_name_ok(name: &str) -> bool {
    !never_forward(name)
}

/// Check the job environment. `node_has` tells whether the peer's own
/// environment defines a name: a declared extra name may not override it.
///
/// `RUSTC_BOOTSTRAP` is accepted because DESIGN section 9 forwards it and it
/// only reaches the job when the submitter set it itself (the job environment
/// is the submitter's forwarded set); a local build with that variable behaves
/// the same. The peer's own `RUSTC_BOOTSTRAP` never reaches a job
/// (`exec::is_build_var`).
pub fn validate_env(spec: &JobSpec, node_has: &dyn Fn(&str) -> bool) -> Result<(), String> {
    for n in &spec.extra_env {
        if !extra_env_name_ok(n) || design_env(n) || node_has(n) {
            return Err("job declares a variable the farm does not forward".into());
        }
    }
    for (k, v) in &spec.env {
        if never_forward(k) && !k.eq_ignore_ascii_case("RUSTC_BOOTSTRAP") {
            return Err("job sets a variable the farm does not forward".into());
        }
        let declared = spec.extra_env.iter().any(|e| e.eq_ignore_ascii_case(k));
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
pub fn validate_spec(spec: &JobSpec, node_has: &dyn Fn(&str) -> bool) -> Result<(), String> {
    check_caps(spec)?;
    validate_args(&spec.args)?;
    validate_env(spec, node_has)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{ClientPath, InputFile, JobId, ToolchainId};

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
        assert_eq!(validate_args(&good()), Ok(()));
        // Attached forms and a mapped linker.
        let mut a = good();
        a.push(Arg::PrefixedPath {
            prefix: "-Clinker=".into(),
            path: cp(r"C:\tools\link.exe"),
        });
        a.push(p("--emit=metadata=target/x.rmeta"));
        a.push(p("-Ldependency=deps"));
        assert_eq!(validate_args(&a), Ok(()));
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
            assert!(validate_args(&a).is_err(), "must refuse {extra:?}");
        }
    }

    #[test]
    fn structure_is_required() {
        let mut a = good();
        a.push(Arg::Path(cp(r"C:\w\src\other.rs")));
        assert!(validate_args(&a).is_err(), "two inputs");
        assert!(
            validate_args(&[p("--crate-type"), p("lib")]).is_err(),
            "no input"
        );
        let mut b = good();
        b.retain(|x| !matches!(x, Arg::Plain(s) if s == "lib"));
        b.retain(|x| !matches!(x, Arg::Plain(s) if s == "--crate-type"));
        assert!(validate_args(&b).is_err(), "no crate type");
        // The marker cannot be forged.
        let mut c = good();
        c.push(p("--cfg"));
        c.push(p("x\u{1}"));
        assert!(validate_args(&c).is_err());
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
        let none = |_: &str| false;
        let ok = spec(
            vec![
                ("CARGO_PKG_NAME", plain("x")),
                ("OUT_DIR", EnvValue::Path(cp(r"C:\o"))),
                ("DEP_A_B", plain("1")),
                ("RUSTC_BOOTSTRAP", plain("1")),
                ("MY_VAR", plain("v")),
            ],
            &["MY_VAR"],
        );
        assert_eq!(validate_env(&ok, &none), Ok(()));
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
            assert!(validate_env(&s, &none).is_err(), "{bad} undeclared");
            // Declaring it does not help.
            let s = spec(vec![(bad, plain("x"))], &[bad]);
            assert!(validate_env(&s, &none).is_err(), "{bad} declared");
        }
        // An undeclared variable is refused; a declared one that the node itself defines too is refused.
        assert!(validate_env(&spec(vec![("MY_VAR", plain("v"))], &[]), &none).is_err());
        assert!(
            validate_env(&spec(vec![("MY_VAR", plain("v"))], &["MY_VAR"]), &|n| n
                == "MY_VAR")
            .is_err()
        );
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
