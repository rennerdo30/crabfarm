//! Package A. Parse and classify a rustc command line (DESIGN section 5).
//!
//! Principle: when in doubt, `Local`. A flag this module does not know is
//! passed through verbatim, unless it carries something that looks like an
//! absolute path, because such a path would not exist on a peer.

use crate::proto::{Arg, ClientPath, PathStyle};
use std::path::Path;

/// Result of classifying one invocation.
// The candidate is the common large case and is consumed once; boxing it would
// only change the interface the other packages build on.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Classified {
    /// Run locally without contacting the node. `why` is for debug logs.
    Local { why: String },
    /// Candidate for remote execution.
    Remote(RemoteCandidate),
}

#[derive(Debug, Clone)]
pub struct RemoteCandidate {
    /// Structured args after argfile expansion, with `--target` added when
    /// absent (`added_target`) and `-C incremental` removed when
    /// `strip_incremental`. Every absolute path is an `Arg::Path`-like variant.
    /// Relative paths stay `Plain` (rustc runs in the mapped client cwd, so
    /// diagnostics keep showing the same relative paths as a local run).
    /// A `-C linker=<absolute path>` is kept as `PrefixedPath` with prefix
    /// `-Clinker=` (or `linker=` after a separate `-C`): the executor decides
    /// whether the file exists on its host and drops the flag if not.
    pub args: Vec<Arg>,
    pub crate_name: String,
    pub crate_types: Vec<String>,
    pub target: String,
    pub added_target: bool,
    /// Primary input `.rs` file.
    pub input: ClientPath,
    pub out_dir: ClientPath,
    /// `--extern` files.
    pub externs: Vec<ClientPath>,
    /// `-L` search dirs with their kind (`dependency`, `native`, `all`, `crate`,
    /// `framework`; `all` when no kind given).
    pub search_dirs: Vec<(String, ClientPath)>,
    /// `-l` libraries: (kind, name), kind `dylib` when none given;
    /// modifiers like `static:+bundle` are kept in kind.
    pub link_libs: Vec<(String, String)>,
    pub emits: Vec<String>,
    pub is_proc_macro: bool,
    /// Library files this invocation will produce in out_dir
    /// (`lib<name><extra-filename>.rlib/.rmeta`, proc-macro library named per
    /// host OS: `<name><extra>.dll`, `lib<name><extra>.so`, `lib<name><extra>.dylib`),
    /// from `--crate-name`, `-C extra-filename`, crate types and emits.
    pub produces: Vec<ClientPath>,
}

pub struct ClassifyOptions<'a> {
    /// Host triple of the local rustc (`rustc -vV` host).
    pub host_triple: &'a str,
    pub cwd: &'a Path,
    pub style: PathStyle,
    /// `remote_incremental = "strip"`.
    pub strip_incremental: bool,
}

/// Expand `@file` arguments (one arg per line, UTF-8, CRLF tolerated).
/// Reads the files; errors are returned so the caller runs locally.
/// Like rustc, expansion is not recursive.
pub fn expand_argfiles(args: &[String]) -> std::io::Result<Vec<String>> {
    let mut out = Vec::with_capacity(args.len());
    for a in args {
        match a.strip_prefix('@') {
            Some(file) if !file.is_empty() => {
                let text = std::fs::read_to_string(file)?;
                // `lines()` drops `\r\n` and `\n` alike and the final terminator.
                out.extend(text.lines().map(str::to_string));
            }
            _ => out.push(a.clone()),
        }
    }
    Ok(out)
}

/// Classify expanded args (without the rustc path).
pub fn classify(args: &[String], opts: &ClassifyOptions) -> Classified {
    match classify_inner(args, opts) {
        Ok(c) => Classified::Remote(c),
        Err(why) => Classified::Local { why },
    }
}

pub(crate) const ALLOWED_EMIT: [&str; 3] = ["dep-info", "metadata", "link"];

/// `-C` keys a job may carry to a peer (CF-19): the ones cargo passes for its
/// profiles (`opt-level`, `debuginfo`, `debug-assertions`, `overflow-checks`,
/// `codegen-units`, `panic`, `strip`, `lto`, `embed-bitcode`, `split-debuginfo`,
/// `metadata`, `extra-filename`, `prefer-dynamic` for proc macros) and the
/// code-generation switches users put into `RUSTFLAGS` or target config. Every
/// other key (`instrument-coverage`, `profile-generate`, `save-temps`, `ar`,
/// `dlltool`, `llvm-args`, `remark`, `passes`, ...) keeps the job local. The
/// path-like ones (`linker`, `link-arg(s)`, `incremental`, `target-cpu=native`)
/// have their own rules in the classifier and in `guard`.
pub(crate) const CODEGEN_REMOTE_OK: [&str; 23] = [
    "opt-level",
    "debuginfo",
    "debug-assertions",
    "overflow-checks",
    "codegen-units",
    "panic",
    "strip",
    "metadata",
    "extra-filename",
    "lto",
    "embed-bitcode",
    "target-feature",
    "target-cpu",
    "relocation-model",
    "code-model",
    "force-frame-pointers",
    "force-unwind-tables",
    "split-debuginfo",
    "symbol-mangling-version",
    "prefer-dynamic",
    "link-dead-code",
    "linker",
    "linker-flavor",
];

/// True when `-C key=...` may run on a peer (see [`CODEGEN_REMOTE_OK`]).
pub(crate) fn codegen_key_remote_ok(key: &str) -> bool {
    CODEGEN_REMOTE_OK.contains(&key)
}
const LINK_CRATE_TYPES: [&str; 4] = ["bin", "dylib", "cdylib", "staticlib"];
pub(crate) const SEARCH_KINDS: [&str; 5] = ["dependency", "crate", "native", "framework", "all"];

/// Long flags that take a value, with the canonical name used for dispatch.
const LONG_VALUE_FLAGS: [(&str, &str); 25] = [
    ("--cfg", "--cfg"),
    ("--check-cfg", "--check-cfg"),
    ("--crate-type", "--crate-type"),
    ("--crate-name", "--crate-name"),
    ("--edition", "--edition"),
    ("--emit", "--emit"),
    ("--error-format", "--error-format"),
    ("--json", "--json"),
    ("--color", "--color"),
    ("--cap-lints", "--cap-lints"),
    ("--target", "--target"),
    ("--out-dir", "--out-dir"),
    ("--extern", "--extern"),
    ("--remap-path-prefix", "--remap-path-prefix"),
    ("--sysroot", "--sysroot"),
    ("--print", "--print"),
    ("--explain", "--explain"),
    ("--codegen", "-C"),
    ("--force-warn", "--force-warn"),
    ("--allow", "-A"),
    ("--warn", "-W"),
    ("--deny", "-D"),
    ("--forbid", "-F"),
    ("--diagnostic-width", "--diagnostic-width"),
    ("--env-set", "--env-set"),
];

/// Short flags that take a value (separate or attached).
const SHORT_VALUE_FLAGS: [&str; 9] = ["-A", "-W", "-D", "-F", "-C", "-L", "-l", "-o", "-Z"];

/// One value-taking flag as written on the command line.
pub(crate) struct Tok {
    /// Canonical name for dispatch (`--codegen` is `-C`).
    pub(crate) name: &'static str,
    pub(crate) value: String,
    /// Text that precedes the value inside the same argument (`--extern=`,
    /// `-L`); empty for the separate form.
    lead: String,
    /// The flag argument itself for the separate form.
    flag_text: Option<String>,
    /// The original argument texts (for verbatim pass-through).
    pub(crate) original: Vec<String>,
}

pub(crate) fn parse_flag(args: &[String], i: usize) -> Result<Option<Tok>, String> {
    let a = &args[i];
    let separate = |name: &'static str| -> Result<Option<Tok>, String> {
        let v = args
            .get(i + 1)
            .ok_or_else(|| format!("{a} without a value"))?;
        Ok(Some(Tok {
            name,
            value: v.clone(),
            lead: String::new(),
            flag_text: Some(a.clone()),
            original: vec![a.clone(), v.clone()],
        }))
    };
    if a.starts_with("--") {
        if let Some((_, canon)) = LONG_VALUE_FLAGS.iter().find(|(l, _)| l == a) {
            return separate(canon);
        }
        if let Some((flag, value)) = a.split_once('=')
            && let Some((_, canon)) = LONG_VALUE_FLAGS.iter().find(|(l, _)| *l == flag)
        {
            return Ok(Some(Tok {
                name: canon,
                value: value.to_string(),
                lead: format!("{flag}="),
                flag_text: None,
                original: vec![a.clone()],
            }));
        }
        return Ok(None);
    }
    if a.len() >= 2 && a.starts_with('-') && a.is_char_boundary(2) {
        let (head, rest) = a.split_at(2);
        if let Some(canon) = SHORT_VALUE_FLAGS.iter().find(|s| **s == head) {
            if rest.is_empty() {
                return separate(canon);
            }
            return Ok(Some(Tok {
                name: canon,
                value: rest.to_string(),
                lead: head.to_string(),
                flag_text: None,
                original: vec![a.clone()],
            }));
        }
    }
    Ok(None)
}

/// Absolute in the sense of "would not mean the same on another machine".
/// Rooted paths without a drive (`\foo`, `/foo` on a Windows client) count as
/// absolute; they then fail to parse and make the job local.
pub(crate) fn looks_absolute(style: PathStyle, s: &str) -> bool {
    let b = s.as_bytes();
    match style {
        PathStyle::Posix => s.starts_with('/'),
        PathStyle::Windows => {
            s.starts_with('\\')
                || s.starts_with('/')
                || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        }
    }
}

struct Ctx<'a> {
    style: PathStyle,
    cwd: ClientPath,
    opts: &'a ClassifyOptions<'a>,
}

impl Ctx<'_> {
    /// Resolve a path argument to an absolute client path; relative values
    /// are joined to the client cwd.
    fn resolve(&self, s: &str) -> Result<ClientPath, String> {
        if s.is_empty() {
            return Err("empty path argument".into());
        }
        let parsed = if looks_absolute(self.style, s) {
            ClientPath::parse(self.style, s)
        } else {
            self.join(&self.cwd, s)
        };
        parsed.ok_or_else(|| "path argument is not a plain absolute path".to_string())
    }

    fn join(&self, dir: &ClientPath, rel: &str) -> Option<ClientPath> {
        let sep = if self.style == PathStyle::Windows {
            '\\'
        } else {
            '/'
        };
        ClientPath::parse(self.style, &format!("{}{sep}{rel}", dir.path))
    }

    /// Build the argument for a path value: absolute values become structured
    /// paths, relative ones stay as written. `kind_prefix` is e.g. `dependency=`.
    fn path_arg(
        &self,
        tok: &Tok,
        kind_prefix: &str,
        path_str: &str,
    ) -> Result<(Arg, ClientPath), String> {
        let cp = self.resolve(path_str)?;
        let prefix = format!("{}{kind_prefix}", tok.lead);
        let arg = if looks_absolute(self.style, path_str) {
            if prefix.is_empty() {
                Arg::Path(cp.clone())
            } else {
                Arg::PrefixedPath {
                    prefix,
                    path: cp.clone(),
                }
            }
        } else {
            Arg::Plain(format!("{prefix}{path_str}"))
        };
        Ok((arg, cp))
    }
}

fn push_flag_and(out: &mut Vec<Arg>, tok: &Tok, value_arg: Arg) {
    if let Some(f) = &tok.flag_text {
        out.push(Arg::Plain(f.clone()));
    }
    out.push(value_arg);
}

fn push_original(out: &mut Vec<Arg>, tok: &Tok) {
    out.extend(tok.original.iter().cloned().map(Arg::Plain));
}

fn classify_inner(args: &[String], opts: &ClassifyOptions) -> Result<RemoteCandidate, String> {
    let cwd_str = opts.cwd.to_str().ok_or("cwd is not UTF-8")?;
    let cwd = ClientPath::parse(opts.style, cwd_str)
        .ok_or("cwd is not an absolute path of the client style")?;
    let cx = Ctx {
        style: opts.style,
        cwd,
        opts,
    };

    let mut out: Vec<Arg> = Vec::with_capacity(args.len() + 2);
    let mut crate_name: Option<String> = None;
    let mut crate_types: Vec<String> = Vec::new();
    let mut target: Option<String> = None;
    let mut input: Option<ClientPath> = None;
    let mut out_dir: Option<ClientPath> = None;
    let mut externs = Vec::new();
    let mut search_dirs = Vec::new();
    let mut link_libs = Vec::new();
    let mut emits: Vec<String> = Vec::new();
    let mut emit_paths: Vec<(String, ClientPath)> = Vec::new();
    let mut o_path: Option<ClientPath> = None;
    let mut extra = String::new();
    let mut link_arg_values: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "-vV" | "-V" | "--version" | "-h" | "--help" => {
                return Err("version or help query".into());
            }
            "--test" => return Err("test harness build".into()),
            "-" => return Err("input from stdin".into()),
            "-g" | "-O" | "-v" | "--verbose" => {
                out.push(Arg::Plain(a.clone()));
                i += 1;
                continue;
            }
            _ => {}
        }
        if a.starts_with('@') {
            return Err("unexpanded @argfile".into());
        }

        if let Some(tok) = parse_flag(args, i)? {
            i += tok.original.len();
            match tok.name {
                "--print" => return Err("--print".into()),
                "--explain" => return Err("--explain".into()),
                "--sysroot" => return Err("--sysroot".into()),
                "-Z" => return Err("-Z flag (allow list is empty)".into()),
                "--crate-type" => {
                    for t in tok
                        .value
                        .split(',')
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                    {
                        if !crate_types.iter().any(|c| c == t) {
                            crate_types.push(t.to_string());
                        }
                    }
                    push_original(&mut out, &tok);
                }
                "--crate-name" => {
                    crate_name = Some(tok.value.clone());
                    push_original(&mut out, &tok);
                }
                "--target" => {
                    let v = &tok.value;
                    if v.ends_with(".json") || v.contains('/') || v.contains('\\') {
                        return Err("custom target specification file".into());
                    }
                    target = Some(v.clone());
                    push_original(&mut out, &tok);
                }
                "--out-dir" => {
                    let (arg, cp) = cx.path_arg(&tok, "", &tok.value)?;
                    out_dir = Some(cp);
                    push_flag_and(&mut out, &tok, arg);
                }
                "-o" => {
                    if tok.value == "-" {
                        return Err("output to stdout".into());
                    }
                    let (arg, cp) = cx.path_arg(&tok, "", &tok.value)?;
                    o_path = Some(cp);
                    push_flag_and(&mut out, &tok, arg);
                }
                "--extern" => {
                    // `[modifier:]name[=path]`; without a path the crate comes from the sysroot.
                    match tok.value.split_once('=') {
                        None => push_original(&mut out, &tok),
                        Some((left, path)) => {
                            let (arg, cp) = cx.path_arg(&tok, &format!("{left}="), path)?;
                            externs.push(cp);
                            push_flag_and(&mut out, &tok, arg);
                        }
                    }
                }
                "-L" => {
                    let (kind, path) = match tok.value.split_once('=') {
                        Some((k, p)) if SEARCH_KINDS.contains(&k) => (k, p),
                        _ => ("all", tok.value.as_str()),
                    };
                    let kind_prefix = if kind == "all" && !tok.value.starts_with("all=") {
                        String::new()
                    } else {
                        format!("{kind}=")
                    };
                    let (arg, cp) = cx.path_arg(&tok, &kind_prefix, path)?;
                    search_dirs.push((kind.to_string(), cp));
                    push_flag_and(&mut out, &tok, arg);
                }
                "-l" => {
                    let (kind, rest) = match tok.value.split_once('=') {
                        Some((k, n)) => (k, n),
                        None => ("dylib", tok.value.as_str()),
                    };
                    // `name:rename` links `name` under another name.
                    let name = rest.split(':').next().unwrap_or(rest);
                    if name.is_empty() {
                        return Err("-l without a name".into());
                    }
                    link_libs.push((kind.to_string(), name.to_string()));
                    push_original(&mut out, &tok);
                }
                "--emit" => {
                    let items: Vec<&str> = tok.value.split(',').filter(|s| !s.is_empty()).collect();
                    let has_path = items.iter().any(|it| it.contains('='));
                    for it in &items {
                        let kind = it.split_once('=').map_or(*it, |(k, _)| k);
                        if !ALLOWED_EMIT.contains(&kind) {
                            return Err(format!("--emit {kind}"));
                        }
                        if !emits.iter().any(|e| e == kind) {
                            emits.push(kind.to_string());
                        }
                    }
                    if has_path {
                        // One flag per item, so each path is its own structured argument.
                        for it in items {
                            match it.split_once('=') {
                                None => out.push(Arg::Plain(format!("--emit={it}"))),
                                Some((kind, path)) => {
                                    let cp = cx.resolve(path)?;
                                    let prefix = format!("--emit={kind}=");
                                    out.push(if looks_absolute(cx.style, path) {
                                        Arg::PrefixedPath {
                                            prefix,
                                            path: cp.clone(),
                                        }
                                    } else {
                                        Arg::Plain(format!("{prefix}{path}"))
                                    });
                                    emit_paths.push((kind.to_string(), cp));
                                }
                            }
                        }
                    } else {
                        push_original(&mut out, &tok);
                    }
                }
                "--remap-path-prefix" => {
                    // rustc splits FROM=TO at the last `=`.
                    let from = tok.value.rsplit_once('=').and_then(|(from, to)| {
                        let cp = looks_absolute(cx.style, from)
                            .then(|| ClientPath::parse(cx.style, from))
                            .flatten()?;
                        Some(Arg::RemapFrom {
                            from: cp,
                            to: to.to_string(),
                        })
                    });
                    match from {
                        Some(arg) => {
                            out.push(Arg::Plain("--remap-path-prefix".into()));
                            out.push(arg);
                        }
                        None => push_original(&mut out, &tok),
                    }
                }
                "-C" => {
                    let (key, val) = tok
                        .value
                        .split_once('=')
                        .unwrap_or((tok.value.as_str(), ""));
                    match key {
                        "incremental" => {
                            if cx.opts.strip_incremental {
                                // Dropped for the remote run; the local fallback still has it.
                            } else {
                                return Err("-C incremental".into());
                            }
                        }
                        "target-cpu" if val == "native" => {
                            return Err("-C target-cpu=native".into());
                        }
                        "profile-generate" | "profile-use" => return Err(format!("-C {key}")),
                        "linker" if looks_absolute(cx.style, val) => {
                            let (arg, _) = cx.path_arg(&tok, "linker=", val)?;
                            push_flag_and(&mut out, &tok, arg);
                        }
                        "extra-filename" => {
                            extra = val.to_string();
                            push_original(&mut out, &tok);
                        }
                        // Kept verbatim; only a proc macro links, and `guard` decides
                        // there (same host, no paths). Checked below once the crate type is known.
                        "link-arg" | "link-args" => {
                            link_arg_values.push(val.to_string());
                            push_original(&mut out, &tok);
                        }
                        _ if !codegen_key_remote_ok(key) => {
                            return Err(format!("-C {key} is not allowed on a peer"));
                        }
                        _ if looks_absolute(cx.style, val) => {
                            return Err(format!("-C {key} takes a path"));
                        }
                        _ => push_original(&mut out, &tok),
                    }
                }
                // `--cfg`, `--edition`, lint levels, error format, ...: nothing to map.
                _ => push_original(&mut out, &tok),
            }
            continue;
        }

        if a.starts_with('-') {
            // Unknown flag: harmless only if no path-like value is attached or follows.
            if let Some((flag, v)) = a.split_once('=')
                && looks_absolute(cx.style, v)
            {
                return Err(format!("unknown flag {flag} with a path value"));
            }
            if let Some(next) = args.get(i + 1)
                && !next.starts_with('-')
                && looks_absolute(cx.style, next)
            {
                return Err(format!("unknown flag {a} followed by a path"));
            }
            out.push(Arg::Plain(a.clone()));
            i += 1;
            continue;
        }

        // Positional: the crate root.
        if input.is_some() {
            return Err("more than one input".into());
        }
        let cp = cx.resolve(a)?;
        out.push(if looks_absolute(cx.style, a) {
            Arg::Path(cp.clone())
        } else {
            Arg::Plain(a.clone())
        });
        input = Some(cp);
        i += 1;
    }

    let input = input.ok_or("no input file")?;
    let crate_name = crate_name
        .filter(|n| !n.is_empty())
        .ok_or("no --crate-name")?;
    let out_dir = out_dir.ok_or("no --out-dir")?;
    // rustc's default is a binary; cargo always passes --crate-type for library builds.
    if crate_types.is_empty() {
        return Err("no --crate-type".into());
    }
    if let Some(t) = crate_types
        .iter()
        .find(|t| LINK_CRATE_TYPES.contains(&t.as_str()))
    {
        return Err(format!("crate type {t} links"));
    }
    if let Some(t) = crate_types
        .iter()
        .find(|t| !matches!(t.as_str(), "lib" | "rlib" | "proc-macro"))
    {
        return Err(format!("unknown crate type {t}"));
    }
    let is_proc_macro = crate_types.iter().any(|t| t == "proc-macro");
    if is_proc_macro && crate_types.len() > 1 {
        return Err("proc-macro mixed with other crate types".into());
    }
    // The executor refuses these anyway; saying so here saves the round trip.
    if is_proc_macro && link_arg_values.iter().any(|v| v.contains(['/', '\\'])) {
        return Err("-C link-arg with a path for a proc macro".into());
    }
    if emits.is_empty() {
        emits.push("link".into());
    }

    let (target, added_target) = match target {
        Some(t) => (t, false),
        None => {
            out.push(Arg::Plain("--target".into()));
            out.push(Arg::Plain(opts.host_triple.to_string()));
            (opts.host_triple.to_string(), true)
        }
    };

    let produces = produces(
        &cx,
        &out_dir,
        &crate_name,
        &extra,
        is_proc_macro,
        &emits,
        &emit_paths,
        o_path,
    )?;

    Ok(RemoteCandidate {
        args: out,
        crate_name,
        crate_types,
        target,
        added_target,
        input,
        out_dir,
        externs,
        search_dirs,
        link_libs,
        emits,
        is_proc_macro,
        produces,
    })
}

#[allow(clippy::too_many_arguments)]
fn produces(
    cx: &Ctx,
    out_dir: &ClientPath,
    name: &str,
    extra: &str,
    is_proc_macro: bool,
    emits: &[String],
    emit_paths: &[(String, ClientPath)],
    o_path: Option<ClientPath>,
) -> Result<Vec<ClientPath>, String> {
    let host = cx.opts.host_triple;
    let proc_macro_file = if host.contains("-windows-") {
        format!("{name}{extra}.dll")
    } else if host.contains("-apple-") {
        format!("lib{name}{extra}.dylib")
    } else {
        format!("lib{name}{extra}.so")
    };
    let mut out: Vec<ClientPath> = Vec::new();
    for kind in emits.iter().filter(|e| *e == "link" || *e == "metadata") {
        let path = match emit_paths.iter().find(|(k, _)| k == kind) {
            Some((_, p)) => p.clone(),
            None => {
                let file = match (kind.as_str(), is_proc_macro) {
                    ("metadata", _) => format!("lib{name}{extra}.rmeta"),
                    (_, true) => proc_macro_file.clone(),
                    _ => format!("lib{name}{extra}.rlib"),
                };
                cx.join(out_dir, &file)
                    .ok_or("cannot build output file name")?
            }
        };
        if !out.contains(&path) {
            out.push(path);
        }
    }
    if let Some(o) = o_path {
        // `-o` names the single output file.
        if out.len() != 1 {
            return Err("-o with more than one output".into());
        }
        out = vec![o];
    }
    Ok(out)
}

/// Render structured args to strings, using `map` for each client path
/// (identity rendering for a local run, sandbox mapping on a peer).
///
/// `classify` always emits `--remap-path-prefix` as its own `Plain` argument
/// followed by one `RemapFrom` holding the value, which renders to
/// `<mapped FROM>=<TO>`.
pub fn render_args(args: &[Arg], map: &dyn Fn(&ClientPath) -> String) -> Vec<String> {
    args.iter()
        .map(|a| match a {
            Arg::Plain(s) => s.clone(),
            Arg::Path(p) => map(p),
            Arg::PrefixedPath { prefix, path } => format!("{prefix}{}", map(path)),
            Arg::RemapFrom { from, to } => format!("{}={to}", map(from)),
        })
        .collect()
}

/// True when an invocation is a build-script probe (CF-42): `OUT_DIR` is set
/// and either `--out-dir` or the primary input file lies under `OUT_DIR`.
/// Build scripts (`anyhow`, `thiserror`, `proc_macro2`, `autocfg`, ...) invoke
/// `rustc` through `RUSTC_WRAPPER` on small compiler-feature probes whose
/// failure on stable is expected; keeping them local avoids wasted round trips
/// and false remote-failure counts.
pub fn is_build_script_probe(cand: &RemoteCandidate, out_dir_env: Option<&str>) -> bool {
    let Some(env_out) = out_dir_env.filter(|s| !s.is_empty()) else {
        return false;
    };
    let Some(env_cp) = ClientPath::parse(cand.out_dir.style, env_out) else {
        return false;
    };
    cand.out_dir.starts_with(&env_cp) || cand.input.starts_with(&env_cp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const WIN_HOST: &str = "x86_64-pc-windows-msvc";

    fn opts(strip: bool) -> ClassifyOptions<'static> {
        ClassifyOptions {
            host_triple: WIN_HOST,
            cwd: Box::leak(Box::new(PathBuf::from(r"E:\w\proj"))),
            style: PathStyle::Windows,
            strip_incremental: strip,
        }
    }

    fn posix_opts(host: &'static str) -> ClassifyOptions<'static> {
        ClassifyOptions {
            host_triple: host,
            cwd: Box::leak(Box::new(PathBuf::from("/w/proj"))),
            style: PathStyle::Posix,
            strip_incremental: false,
        }
    }

    /// Split a line as cargo prints it in `cargo build -v` (single-quoted
    /// when an argument has special characters), without the program name.
    fn cargo_line(line: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut cur = String::new();
        let mut quoted = false;
        let mut any = false;
        for c in line.chars() {
            match c {
                '\'' => {
                    quoted = !quoted;
                    any = true;
                }
                c if c.is_whitespace() && !quoted => {
                    if any || !cur.is_empty() {
                        args.push(std::mem::take(&mut cur));
                        any = false;
                    }
                }
                c => cur.push(c),
            }
        }
        if any || !cur.is_empty() {
            args.push(cur);
        }
        args.remove(0);
        args
    }

    fn v(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    fn remote(args: &[String], o: &ClassifyOptions) -> RemoteCandidate {
        match classify(args, o) {
            Classified::Remote(r) => r,
            Classified::Local { why } => panic!("expected remote, got local: {why}"),
        }
    }

    fn local(args: &[String], o: &ClassifyOptions) -> String {
        match classify(args, o) {
            Classified::Local { why } => why,
            Classified::Remote(r) => panic!("expected local, got remote: {:?}", r.args),
        }
    }

    fn cp(s: &str) -> ClientPath {
        ClientPath::parse(PathStyle::Windows, s).expect("path")
    }

    /// A minimal remote-eligible library invocation to append flags to.
    const BASE: &str = "--crate-name foo --edition=2024 src/lib.rs --crate-type lib --emit=dep-info,metadata,link --out-dir E:/w/tgt/debug/deps";

    // ---- real cargo command lines (cargo 1.100, `cargo build -v`, Windows, shortened target dir) ----

    const BUILD_SCRIPT: &str = r"C:\r\rustc.exe --crate-name build_script_build --edition=2024 'app\build.rs' --error-format=json --json=diagnostic-rendered-ansi,artifacts,future-incompat --crate-type bin --emit=dep-info,link -C embed-bitcode=no --check-cfg 'cfg(docsrs,test)' --check-cfg 'cfg(feature, values())' -C metadata=e56bfc6782fc8e77 -C extra-filename=-39678dbd824befe1 --out-dir 'E:/w/tgt\debug\build\app-39678dbd824befe1' -C 'incremental=E:/w/tgt\debug\incremental' -L 'dependency=E:/w/tgt\debug\deps'";
    const DEP_LIB: &str = r"C:\r\rustc.exe --crate-name dep --edition=2024 'dep\src\lib.rs' --error-format=json --json=diagnostic-rendered-ansi,artifacts,future-incompat --crate-type lib --emit=dep-info,metadata,link -C embed-bitcode=no -C debuginfo=2 --check-cfg 'cfg(docsrs,test)' --check-cfg 'cfg(feature, values())' -C metadata=07105916f43eead2 -C extra-filename=-509a93a483eb2c07 --out-dir 'E:/w/tgt\debug\deps' -C 'incremental=E:/w/tgt\debug\incremental' -L 'dependency=E:/w/tgt\debug\deps'";
    const PM_LIB: &str = r"C:\r\rustc.exe --crate-name pm --edition=2024 'pm\src\lib.rs' --error-format=json --json=diagnostic-rendered-ansi,artifacts,future-incompat --crate-type proc-macro --emit=dep-info,link -C prefer-dynamic -C embed-bitcode=no -C debuginfo=2 --check-cfg 'cfg(docsrs,test)' --check-cfg 'cfg(feature, values())' -C metadata=5dcea8d5015ce7b5 -C extra-filename=-9086cc66bfef8fef --out-dir 'E:/w/tgt\debug\deps' -C 'incremental=E:/w/tgt\debug\incremental' -L 'dependency=E:/w/tgt\debug\deps' --extern proc_macro";
    const APP_LIB: &str = r"C:\r\rustc.exe --crate-name app --edition=2024 'app\src\lib.rs' --error-format=json --json=diagnostic-rendered-ansi,artifacts,future-incompat --crate-type lib --emit=dep-info,metadata,link -C embed-bitcode=no -C debuginfo=2 --check-cfg 'cfg(docsrs,test)' --check-cfg 'cfg(feature, values())' -C metadata=05adc2232873e420 -C extra-filename=-c1ffdcf5a17328db --out-dir 'E:/w/tgt\debug\deps' -C 'incremental=E:/w/tgt\debug\incremental' -L 'dependency=E:/w/tgt\debug\deps' --extern 'dep=E:/w/tgt\debug\deps\libdep-509a93a483eb2c07.rmeta' --extern 'pm=E:/w/tgt\debug\deps\pm-9086cc66bfef8fef.dll'";
    const APP_BIN: &str = r"C:\r\rustc.exe --crate-name app --edition=2024 'app\src\main.rs' --error-format=json --json=diagnostic-rendered-ansi,artifacts,future-incompat --crate-type bin --emit=dep-info,link -C embed-bitcode=no -C debuginfo=2 --check-cfg 'cfg(docsrs,test)' --check-cfg 'cfg(feature, values())' -C metadata=62a098b037fee4ce --out-dir 'E:/w/tgt\debug\deps' -C 'incremental=E:/w/tgt\debug\incremental' -L 'dependency=E:/w/tgt\debug\deps' --extern 'app=E:/w/tgt\debug\deps\libapp-c1ffdcf5a17328db.rlib' --extern 'dep=E:/w/tgt\debug\deps\libdep-509a93a483eb2c07.rlib' --extern 'pm=E:/w/tgt\debug\deps\pm-9086cc66bfef8fef.dll'";
    const APP_TEST: &str = r"C:\r\rustc.exe --crate-name app --edition=2024 'app\src\lib.rs' --error-format=json --json=diagnostic-rendered-ansi,artifacts,future-incompat --emit=dep-info,link -C embed-bitcode=no -C debuginfo=2 --test --check-cfg 'cfg(docsrs,test)' --check-cfg 'cfg(feature, values())' -C metadata=31d1ab6976bd2c86 -C extra-filename=-fa18129c5630e1d7 --out-dir 'E:/w/tgt\debug\deps' -C 'incremental=E:/w/tgt\debug\incremental' -L 'dependency=E:/w/tgt\debug\deps' --extern 'dep=E:/w/tgt\debug\deps\libdep-509a93a483eb2c07.rlib' --extern 'pm=E:/w/tgt\debug\deps\pm-e2e38c5b8475d878.dll'";
    const REL_DEP_LIB: &str = r"C:\r\rustc.exe --crate-name dep --edition=2024 'dep\src\lib.rs' --error-format=json --json=diagnostic-rendered-ansi,artifacts,future-incompat --crate-type lib --emit=dep-info,metadata,link -C opt-level=3 -C embed-bitcode=no --check-cfg 'cfg(docsrs,test)' --check-cfg 'cfg(feature, values())' -C metadata=56082762f02d926a -C extra-filename=-2b59048b2ad17840 --out-dir 'E:/w/tgt\release\deps' -C strip=debuginfo -L 'dependency=E:/w/tgt\release\deps'";

    #[test]
    fn real_line_build_script_is_local() {
        let why = local(&cargo_line(BUILD_SCRIPT), &opts(true));
        assert!(why.contains("bin"), "{why}");
    }

    #[test]
    fn real_line_app_bin_is_local() {
        assert!(local(&cargo_line(APP_BIN), &opts(true)).contains("bin"));
    }

    #[test]
    fn real_line_test_harness_is_local() {
        assert!(local(&cargo_line(APP_TEST), &opts(true)).contains("test"));
    }

    #[test]
    fn real_line_debug_lib_is_local_unless_incremental_is_stripped() {
        assert!(local(&cargo_line(DEP_LIB), &opts(false)).contains("incremental"));
        let r = remote(&cargo_line(DEP_LIB), &opts(true));
        assert_eq!(r.crate_name, "dep");
        assert_eq!(r.crate_types, vec!["lib"]);
        assert_eq!(r.emits, vec!["dep-info", "metadata", "link"]);
        assert!(r.added_target);
        assert_eq!(r.target, WIN_HOST);
        assert_eq!(r.input, cp(r"E:\w\proj\dep\src\lib.rs"));
        assert_eq!(r.out_dir, cp(r"E:\w\tgt\debug\deps"));
        assert_eq!(
            r.search_dirs,
            vec![("dependency".to_string(), cp(r"E:\w\tgt\debug\deps"))]
        );
        assert!(!r.is_proc_macro);
        assert_eq!(
            r.produces,
            vec![
                cp(r"E:\w\tgt\debug\deps\libdep-509a93a483eb2c07.rmeta"),
                cp(r"E:\w\tgt\debug\deps\libdep-509a93a483eb2c07.rlib"),
            ]
        );
        // The incremental flag is gone in both parts, everything else is there.
        let flat = render_args(&r.args, &|p| p.path.clone());
        assert!(!flat.iter().any(|a| a.contains("incremental")), "{flat:?}");
        assert!(flat.windows(2).any(|w| w == ["--target", WIN_HOST]));
        assert!(flat.contains(&"embed-bitcode=no".to_string()));
        // The input stays relative, as cargo passed it.
        assert!(flat.contains(&r"dep\src\lib.rs".to_string()));
        // Absolute paths are structured.
        assert!(r.args.contains(&Arg::PrefixedPath {
            prefix: "dependency=".into(),
            path: cp(r"E:\w\tgt\debug\deps")
        }));
    }

    #[test]
    fn real_line_release_lib_is_remote_without_stripping() {
        let r = remote(&cargo_line(REL_DEP_LIB), &opts(false));
        assert_eq!(r.out_dir, cp(r"E:\w\tgt\release\deps"));
        assert!(r.externs.is_empty());
    }

    #[test]
    fn real_line_proc_macro() {
        let r = remote(&cargo_line(PM_LIB), &opts(true));
        assert!(r.is_proc_macro);
        assert_eq!(r.crate_types, vec!["proc-macro"]);
        // `--extern proc_macro` has no path.
        assert!(r.externs.is_empty());
        assert_eq!(
            r.produces,
            vec![cp(r"E:\w\tgt\debug\deps\pm-9086cc66bfef8fef.dll")]
        );
        assert_eq!(r.emits, vec!["dep-info", "link"]);
    }

    #[test]
    fn real_line_lib_with_externs() {
        let r = remote(&cargo_line(APP_LIB), &opts(true));
        assert_eq!(
            r.externs,
            vec![
                cp(r"E:\w\tgt\debug\deps\libdep-509a93a483eb2c07.rmeta"),
                cp(r"E:\w\tgt\debug\deps\pm-9086cc66bfef8fef.dll"),
            ]
        );
        assert!(r.args.contains(&Arg::PrefixedPath {
            prefix: "dep=".into(),
            path: cp(r"E:\w\tgt\debug\deps\libdep-509a93a483eb2c07.rmeta")
        }));
        // Value of the separate `--extern` is preceded by its own flag argument.
        let flat = render_args(&r.args, &|p| p.path.clone());
        assert!(
            flat.windows(2)
                .any(|w| w[0] == "--extern" && w[1].starts_with("pm=E:")),
            "{flat:?}"
        );
    }

    // ---- always local ----

    #[test]
    fn local_version_queries_and_prints() {
        let o = opts(true);
        for a in ["-vV", "--version", "-V", "--help", "-h"] {
            assert!(!local(&v(a), &o).is_empty(), "{a}");
        }
        local(&v("--print cfg"), &o);
        local(&v("--print=cfg"), &o);
        local(
            &v("--crate-name x --print file-names --crate-type lib src/lib.rs"),
            &o,
        );
    }

    #[test]
    fn local_stdin_and_missing_input() {
        let o = opts(true);
        local(&v("--crate-name foo --crate-type lib --out-dir E:/o -"), &o);
        local(&v("--crate-name foo --crate-type lib --out-dir E:/o"), &o);
        local(&[], &o);
    }

    #[test]
    fn local_linking_crate_types() {
        let o = opts(true);
        for t in ["bin", "dylib", "cdylib", "staticlib"] {
            let line = format!("--crate-name foo src/lib.rs --crate-type {t} --out-dir E:/o");
            let why = local(&v(&line), &o);
            assert!(why.contains(t), "{why}");
        }
        // Linking type anywhere in a repeated or comma separated list.
        local(
            &v("--crate-name foo src/lib.rs --crate-type lib --crate-type=cdylib --out-dir E:/o"),
            &o,
        );
        local(
            &v("--crate-name foo src/lib.rs --crate-type=rlib,dylib --out-dir E:/o"),
            &o,
        );
    }

    #[test]
    fn local_test_harness() {
        local(&v(&format!("{BASE} --test")), &opts(true));
    }

    #[test]
    fn local_unsupported_emit() {
        let o = opts(true);
        for e in [
            "asm",
            "llvm-ir",
            "llvm-bc",
            "obj",
            "mir",
            "link,asm",
            "dep-info,obj=E:/o/x.o",
        ] {
            let line =
                format!("--crate-name foo src/lib.rs --crate-type lib --emit={e} --out-dir E:/o");
            local(&v(&line), &o);
        }
    }

    #[test]
    fn local_incremental_all_spellings() {
        let o = opts(false);
        local(&v(&format!("{BASE} -C incremental=E:/i")), &o);
        local(&v(&format!("{BASE} -Cincremental=E:/i")), &o);
        local(&v(&format!("{BASE} --codegen=incremental=E:/i")), &o);
        local(&v(&format!("{BASE} --codegen incremental=E:/i")), &o);
    }

    #[test]
    fn strip_incremental_removes_every_spelling() {
        let o = opts(true);
        for inc in [
            "-C incremental=E:/i",
            "-Cincremental=E:/i",
            "--codegen=incremental=E:/i",
            "--codegen incremental=E:/i",
        ] {
            let r = remote(&v(&format!("{BASE} {inc} -C opt-level=3")), &o);
            let flat = render_args(&r.args, &|p| p.path.clone());
            assert!(
                !flat
                    .iter()
                    .any(|a| a.contains("incremental") || a.contains("E:/i")),
                "{inc}: {flat:?}"
            );
            assert!(flat.contains(&"opt-level=3".to_string()), "{inc}: {flat:?}");
        }
    }

    #[test]
    fn local_native_cpu_and_pgo() {
        let o = opts(true);
        local(&v(&format!("{BASE} -C target-cpu=native")), &o);
        local(&v(&format!("{BASE} -Ctarget-cpu=native")), &o);
        local(&v(&format!("{BASE} -C profile-generate")), &o);
        local(&v(&format!("{BASE} -C profile-generate=E:/pgo")), &o);
        local(&v(&format!("{BASE} -Cprofile-use=E:/pgo/x.profdata")), &o);
        // Other target-cpu values are fine.
        remote(&v(&format!("{BASE} -C target-cpu=x86-64-v3")), &o);
    }

    #[test]
    fn local_z_flags_and_sysroot() {
        let o = opts(true);
        local(&v(&format!("{BASE} -Z macro-backtrace")), &o);
        local(&v(&format!("{BASE} -Zmacro-backtrace")), &o);
        local(&v(&format!("{BASE} --sysroot E:/sys")), &o);
        local(&v(&format!("{BASE} --sysroot=E:/sys")), &o);
    }

    #[test]
    fn unknown_flags_depend_on_path_like_values() {
        let o = opts(true);
        local(&v(&format!("{BASE} --mystery=E:/x")), &o);
        local(&v(&format!("{BASE} --mystery C:\\x\\y")), &o);
        local(&v(&format!("{BASE} --mystery //srv/share/x")), &o);
        local(&v(&format!("{BASE} --mystery /rooted")), &o);
        let r = remote(&v(&format!("{BASE} --mystery=value --other")), &o);
        assert!(r.args.contains(&Arg::Plain("--mystery=value".into())));
        // Posix client: a leading slash is absolute.
        let p = posix_opts("x86_64-unknown-linux-gnu");
        local(
            &v("--crate-name foo src/lib.rs --crate-type lib --out-dir /o --mystery=/abs"),
            &p,
        );
        remote(
            &v("--crate-name foo src/lib.rs --crate-type lib --out-dir /o --mystery=abs"),
            &p,
        );
    }

    #[test]
    fn local_path_valued_codegen_option() {
        local(&v(&format!("{BASE} -C ar=E:/tools/ar.exe")), &opts(true));
    }

    #[test]
    fn local_missing_pieces() {
        let o = opts(true);
        local(&v("--crate-type lib --out-dir E:/o src/lib.rs"), &o); // no crate name
        local(&v("--crate-name foo --crate-type lib src/lib.rs"), &o); // no out dir
        local(&v("--crate-name foo --out-dir E:/o src/lib.rs"), &o); // no crate type: rustc would build a bin
        local(&v(&format!("{BASE} other.rs")), &o); // two inputs
        local(&v(&format!("{BASE} @leftover.args")), &o);
        local(&v(&format!("{BASE} --target E:/spec/my.json")), &o);
        local(&v(&format!("{BASE} --crate-type")), &o); // flag without value
    }

    #[test]
    fn local_unknown_or_mixed_crate_types() {
        let o = opts(true);
        local(
            &v("--crate-name foo src/lib.rs --crate-type weird --out-dir E:/o"),
            &o,
        );
        local(
            &v(
                "--crate-name foo src/lib.rs --crate-type proc-macro --crate-type lib --out-dir E:/o",
            ),
            &o,
        );
    }

    #[test]
    fn local_drive_relative_and_dotdot_escapes() {
        let o = opts(true);
        local(
            &v("--crate-name foo C:lib.rs --crate-type lib --out-dir E:/o"),
            &o,
        );
        local(
            &v("--crate-name foo ../../../../x.rs --crate-type lib --out-dir E:/o"),
            &o,
        );
    }

    // ---- accepted spellings ----

    #[test]
    fn crate_types_repeated_and_comma_separated() {
        let o = opts(true);
        let r = remote(
            &v("--crate-name foo src/lib.rs --crate-type lib --crate-type rlib --out-dir E:/o"),
            &o,
        );
        assert_eq!(r.crate_types, vec!["lib", "rlib"]);
        let r = remote(
            &v("--crate-name foo src/lib.rs --crate-type=lib,rlib --out-dir E:/o"),
            &o,
        );
        assert_eq!(r.crate_types, vec!["lib", "rlib"]);
        // Both produce the same single rlib.
        assert_eq!(r.produces.len(), 1);
    }

    #[test]
    fn emit_spellings() {
        let o = opts(true);
        let b = "--crate-name foo src/lib.rs --crate-type lib --out-dir E:/o";
        assert_eq!(
            remote(&v(&format!("{b} --emit=metadata")), &o).emits,
            vec!["metadata"]
        );
        assert_eq!(
            remote(&v(&format!("{b} --emit metadata,link")), &o).emits,
            vec!["metadata", "link"]
        );
        // Absent: rustc defaults to link.
        let r = remote(&v(b), &o);
        assert_eq!(r.emits, vec!["link"]);
        assert_eq!(r.produces, vec![cp(r"E:\o\libfoo.rlib")]);
        // Explicit output paths are mapped and become the produced file.
        let r = remote(&v(&format!("{b} --emit=link=E:/x/foo.rlib,dep-info")), &o);
        assert_eq!(r.emits, vec!["link", "dep-info"]);
        assert_eq!(r.produces, vec![cp(r"E:\x\foo.rlib")]);
        assert!(r.args.contains(&Arg::PrefixedPath {
            prefix: "--emit=link=".into(),
            path: cp(r"E:\x\foo.rlib")
        }));
        assert!(r.args.contains(&Arg::Plain("--emit=dep-info".into())));
    }

    #[test]
    fn extern_spellings() {
        let o = opts(true);
        let b = "--crate-name foo src/lib.rs --crate-type lib --out-dir E:/o";
        let r = remote(&v(&format!("{b} --extern serde --extern=std2")), &o);
        assert!(r.externs.is_empty());
        let r = remote(
            &v(&format!(
                "{b} --extern a=E:/d/liba.rlib --extern=b=E:/d/libb.rmeta"
            )),
            &o,
        );
        assert_eq!(
            r.externs,
            vec![cp(r"E:\d\liba.rlib"), cp(r"E:\d\libb.rmeta")]
        );
        assert!(r.args.contains(&Arg::PrefixedPath {
            prefix: "--extern=b=".into(),
            path: cp(r"E:\d\libb.rmeta")
        }));
        let r = remote(
            &v(&format!(
                "{b} --extern priv:a=E:/d/liba.rlib --extern noprelude:b=E:/d/libb.rlib"
            )),
            &o,
        );
        assert_eq!(r.externs.len(), 2);
        assert!(r.args.contains(&Arg::PrefixedPath {
            prefix: "priv:a=".into(),
            path: cp(r"E:\d\liba.rlib")
        }));
        // A relative extern path is resolved for the input list but stays relative in the args.
        let r = remote(&v(&format!("{b} --extern a=deps/liba.rlib")), &o);
        assert_eq!(r.externs, vec![cp(r"E:\w\proj\deps\liba.rlib")]);
        assert!(r.args.contains(&Arg::Plain("a=deps/liba.rlib".into())));
    }

    #[test]
    fn search_dir_spellings() {
        let o = opts(true);
        let b = "--crate-name foo src/lib.rs --crate-type lib --out-dir E:/o";
        let r = remote(
            &v(&format!(
                "{b} -L E:/a -LE:/b -L native=E:/c -Ldependency=E:/d -L all=E:/e -L framework=E:/f"
            )),
            &o,
        );
        let got: Vec<(String, String)> = r
            .search_dirs
            .iter()
            .map(|(k, p)| (k.clone(), p.path.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("all".into(), r"E:\a".into()),
                ("all".into(), r"E:\b".into()),
                ("native".into(), r"E:\c".into()),
                ("dependency".into(), r"E:\d".into()),
                ("all".into(), r"E:\e".into()),
                ("framework".into(), r"E:\f".into()),
            ]
        );
        // Spelling is preserved when rendered.
        let flat = render_args(&r.args, &|p| p.path.clone());
        for s in [
            r"E:\a",
            r"-LE:\b",
            r"native=E:\c",
            r"-Ldependency=E:\d",
            r"all=E:\e",
        ] {
            assert!(flat.contains(&s.to_string()), "{s} in {flat:?}");
        }
        // A path that merely contains `=` after a non-kind prefix is a plain path (Posix).
        let p = posix_opts("x86_64-unknown-linux-gnu");
        let r = remote(
            &v("--crate-name foo src/lib.rs --crate-type lib --out-dir /o -L /a=b/c"),
            &p,
        );
        assert_eq!(r.search_dirs[0].0, "all");
        assert_eq!(r.search_dirs[0].1.path, "/a=b/c");
    }

    #[test]
    fn codegen_spellings_keep_form() {
        let o = opts(true);
        let r = remote(
            &v(&format!(
                "{BASE} -C opt-level=3 -Copt-level=2 --codegen opt-level=1 --codegen=lto=thin"
            )),
            &o,
        );
        let flat = render_args(&r.args, &|p| p.path.clone());
        for s in [
            "opt-level=3",
            "-Copt-level=2",
            "opt-level=1",
            "--codegen=lto=thin",
        ] {
            assert!(flat.contains(&s.to_string()), "{s} in {flat:?}");
        }
    }

    #[test]
    fn link_lib_spellings() {
        let o = opts(true);
        let b = "--crate-name foo src/lib.rs --crate-type lib --out-dir E:/o";
        let r = remote(
            &v(&format!(
                "{b} -l z -lm -l static=foo -l static:+bundle=bar -l dylib=baz:renamed -lstatic:-bundle,+whole-archive=q"
            )),
            &o,
        );
        let got: Vec<(&str, &str)> = r
            .link_libs
            .iter()
            .map(|(k, n)| (k.as_str(), n.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("dylib", "z"),
                ("dylib", "m"),
                ("static", "foo"),
                ("static:+bundle", "bar"),
                ("dylib", "baz"),
                ("static:-bundle,+whole-archive", "q"),
            ]
        );
    }

    #[test]
    fn linker_flag() {
        let o = opts(true);
        // Bare name stays plain.
        let r = remote(&v(&format!("{BASE} -C linker=rust-lld")), &o);
        assert!(r.args.contains(&Arg::Plain("linker=rust-lld".into())));
        // Absolute path stays structured for the executor to decide.
        let r = remote(&v(&format!("{BASE} -Clinker=E:/t/lld.exe")), &o);
        assert!(r.args.contains(&Arg::PrefixedPath {
            prefix: "-Clinker=".into(),
            path: cp(r"E:\t\lld.exe")
        }));
        let r = remote(
            &v(&format!(
                "{BASE} -C link-arg=E:/x/y.lib -C link-args=-fuse-ld=lld"
            )),
            &o,
        );
        assert!(r.args.contains(&Arg::Plain("link-arg=E:/x/y.lib".into())));
    }

    #[test]
    fn remap_path_prefix() {
        let o = opts(true);
        let r = remote(&v(&format!("{BASE} --remap-path-prefix E:/w=/src")), &o);
        assert!(
            r.args
                .windows(2)
                .any(|w| w[0] == Arg::Plain("--remap-path-prefix".into())
                    && w[1]
                        == Arg::RemapFrom {
                            from: cp(r"E:\w"),
                            to: "/src".into()
                        })
        );
        let r = remote(&v(&format!("{BASE} --remap-path-prefix=E:/w=/src")), &o);
        assert!(r.args.iter().any(|a| matches!(a, Arg::RemapFrom { .. })));
        // A FROM that is not a client path stays verbatim.
        let r = remote(
            &v(&format!("{BASE} --remap-path-prefix=/rustc/abc=/rustc")),
            &o,
        );
        assert!(
            r.args
                .contains(&Arg::Plain("--remap-path-prefix=/rustc/abc=/rustc".into()))
        );
        // Rendering maps FROM only.
        let flat = render_args(
            &remote(&v(&format!("{BASE} --remap-path-prefix E:/w=E:/keep")), &o).args,
            &|p| format!("<{}>", p.path),
        );
        assert!(flat.contains(&r"<E:\w>=E:/keep".to_string()), "{flat:?}");
    }

    #[test]
    fn dash_o_maps_single_output() {
        let o = opts(true);
        let r = remote(
            &v(
                "--crate-name foo src/lib.rs --crate-type rlib --emit=link -o E:/o/libfoo.rlib --out-dir E:/o",
            ),
            &o,
        );
        assert_eq!(r.produces, vec![cp(r"E:\o\libfoo.rlib")]);
        assert!(r.args.contains(&Arg::Path(cp(r"E:\o\libfoo.rlib"))));
        local(
            &v(
                "--crate-name foo src/lib.rs --crate-type rlib --emit=metadata,link -o E:/o/x --out-dir E:/o",
            ),
            &o,
        );
    }

    #[test]
    fn explicit_target_is_kept_not_added() {
        let o = opts(true);
        let r = remote(&v(&format!("{BASE} --target x86_64-unknown-linux-gnu")), &o);
        assert!(!r.added_target);
        assert_eq!(r.target, "x86_64-unknown-linux-gnu");
        let r = remote(&v(&format!("{BASE} --target=aarch64-apple-darwin")), &o);
        assert_eq!(r.target, "aarch64-apple-darwin");
        assert_eq!(
            render_args(&r.args, &|p| p.path.clone())
                .iter()
                .filter(|a| a.contains("--target"))
                .count(),
            1
        );
    }

    #[test]
    fn absolute_input_is_a_structured_path() {
        let o = opts(true);
        let r = remote(
            &v("--crate-name foo E:/w/proj/src/lib.rs --crate-type lib --out-dir E:/o"),
            &o,
        );
        assert_eq!(r.input, cp(r"E:\w\proj\src\lib.rs"));
        assert!(r.args.contains(&Arg::Path(cp(r"E:\w\proj\src\lib.rs"))));
        // Relative input resolves against the cwd.
        let r = remote(
            &v("--crate-name foo src/lib.rs --crate-type lib --out-dir E:/o"),
            &o,
        );
        assert_eq!(r.input, cp(r"E:\w\proj\src\lib.rs"));
    }

    #[test]
    fn unc_paths_are_structured() {
        let o = opts(true);
        let r = remote(
            &v(r"--crate-name foo //srv/share/p/lib.rs --crate-type lib --out-dir \\srv\share\o"),
            &o,
        );
        assert_eq!(r.input, cp(r"\\srv\share\p\lib.rs"));
        assert_eq!(r.out_dir, cp(r"\\srv\share\o"));
    }

    // ---- produces per host ----

    #[test]
    fn produces_proc_macro_per_host() {
        let line = "--crate-name pm src/lib.rs --crate-type proc-macro --emit=dep-info,link,metadata -C extra-filename=-abc --out-dir /o";
        for (host, file) in [
            ("x86_64-pc-windows-msvc", "pm-abc.dll"),
            ("aarch64-pc-windows-msvc", "pm-abc.dll"),
            ("aarch64-apple-darwin", "libpm-abc.dylib"),
            ("x86_64-unknown-linux-gnu", "libpm-abc.so"),
            ("x86_64-unknown-freebsd", "libpm-abc.so"),
        ] {
            let r = remote(&v(line), &posix_opts(host));
            let names: Vec<&str> = r
                .produces
                .iter()
                .map(|p| p.path.rsplit('/').next().unwrap_or(""))
                .collect();
            assert_eq!(names, vec![file, "libpm-abc.rmeta"], "{host}");
        }
    }

    #[test]
    fn produces_library_only_not_dep_info() {
        let r = remote(&v(&format!("{BASE} -C extra-filename=-x1")), &opts(true));
        let names: Vec<&str> = r
            .produces
            .iter()
            .map(|p| p.path.rsplit('\\').next().unwrap_or(""))
            .collect();
        assert_eq!(names, vec!["libfoo-x1.rmeta", "libfoo-x1.rlib"]);
        let r = remote(
            &v("--crate-name foo src/lib.rs --crate-type lib --emit=dep-info --out-dir E:/o"),
            &opts(true),
        );
        assert!(r.produces.is_empty());
    }

    // ---- argfiles ----

    #[test]
    fn argfile_expansion_lf_and_crlf() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lf = dir.path().join("lf.args");
        let crlf = dir.path().join("crlf.args");
        std::fs::write(&lf, "--crate-name\nfoo\n--cfg\nfeature=\"a b\"\n").expect("write");
        std::fs::write(&crlf, "--edition=2024\r\nsrc/lib.rs\r\n").expect("write");
        let args = vec![
            format!("@{}", lf.display()),
            "--crate-type".to_string(),
            "lib".to_string(),
            format!("@{}", crlf.display()),
        ];
        let out = expand_argfiles(&args).expect("expand");
        assert_eq!(
            out,
            [
                "--crate-name",
                "foo",
                "--cfg",
                "feature=\"a b\"",
                "--crate-type",
                "lib",
                "--edition=2024",
                "src/lib.rs"
            ]
        );
    }

    #[test]
    fn argfile_missing_and_non_utf8_are_errors() {
        assert!(expand_argfiles(&["@does-not-exist.args".to_string()]).is_err());
        let dir = tempfile::tempdir().expect("tempdir");
        let bad = dir.path().join("bad.args");
        std::fs::write(&bad, [0xff, 0xfe, b'\n']).expect("write");
        assert!(expand_argfiles(&[format!("@{}", bad.display())]).is_err());
    }

    #[test]
    fn argfile_plain_args_and_lone_at_pass_through() {
        let a = vec![
            "--crate-name".to_string(),
            "@".to_string(),
            "x@y".to_string(),
        ];
        assert_eq!(expand_argfiles(&a).expect("expand"), a);
    }

    #[test]
    fn argfile_expanded_line_classifies_like_inline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("a.args");
        let body: String = cargo_line(REL_DEP_LIB)
            .into_iter()
            .map(|a| a + "\n")
            .collect();
        std::fs::write(&f, body).expect("write");
        let expanded = expand_argfiles(&[format!("@{}", f.display())]).expect("expand");
        let r = remote(&expanded, &opts(false));
        assert_eq!(r.crate_name, "dep");
    }

    // ---- render ----

    #[test]
    fn render_maps_every_path_variant() {
        let p = |s: &str| ClientPath::parse(PathStyle::Windows, s).expect("path");
        let args = vec![
            Arg::Plain("--crate-name".into()),
            Arg::Plain("foo".into()),
            Arg::Path(p(r"C:\a\lib.rs")),
            Arg::PrefixedPath {
                prefix: "--out-dir=".into(),
                path: p(r"C:\a\out"),
            },
            Arg::PrefixedPath {
                prefix: "foo=".into(),
                path: p(r"C:\a\libfoo.rlib"),
            },
            Arg::Plain("--remap-path-prefix".into()),
            Arg::RemapFrom {
                from: p(r"C:\a"),
                to: "/x".into(),
            },
        ];
        let out = render_args(&args, &|c| format!("<{}>", c.path));
        assert_eq!(
            out,
            vec![
                "--crate-name",
                "foo",
                r"<C:\a\lib.rs>",
                r"--out-dir=<C:\a\out>",
                r"foo=<C:\a\libfoo.rlib>",
                "--remap-path-prefix",
                r"<C:\a>=/x",
            ]
        );
    }

    #[test]
    fn identity_render_reproduces_a_local_command_line() {
        // Without strip and with a given target nothing is added or removed, so the
        // rendering is the original line up to path normalization.
        let o = opts(false);
        let line = cargo_line(REL_DEP_LIB);
        let mut with_target = line.clone();
        with_target.extend(["--target".to_string(), WIN_HOST.to_string()]);
        let r = remote(&with_target, &o);
        let rendered = render_args(&r.args, &|p| p.path.clone());
        assert_eq!(rendered.len(), with_target.len());
        let norm = |s: &str| s.replace('/', r"\");
        for (got, want) in rendered.iter().zip(&with_target) {
            assert_eq!(norm(got), norm(want));
        }
    }

    /// CF-42: build scripts (`anyhow`, `thiserror`, `proc_macro2`, `autocfg`)
    /// probe the compiler with `--out-dir` or the source file inside `OUT_DIR`.
    /// Real crates with a build script have `--out-dir` outside `OUT_DIR`.
    #[test]
    fn build_script_probes_inside_out_dir_are_detected() {
        let o = opts(true);
        let out_dir = r"E:\w\tgt\debug\build\anyhow-123456\out";
        // anyhow / thiserror style: package probe source, --out-dir = OUT_DIR.
        let anyhow_probe = remote(
            &v("--crate-name anyhow --edition=2018 build/probe.rs --crate-type lib --emit=dep-info,metadata --out-dir E:/w/tgt/debug/build/anyhow-123456/out"),
            &o,
        );
        assert!(is_build_script_probe(&anyhow_probe, Some(out_dir)));
        assert!(!is_build_script_probe(&anyhow_probe, None));

        // autocfg style: source file inside OUT_DIR too.
        let autocfg_probe = remote(
            &v("--crate-name probe1 E:/w/tgt/debug/build/anyhow-123456/out/probe1.rs --crate-type lib --out-dir E:/w/tgt/debug/deps"),
            &o,
        );
        assert!(is_build_script_probe(&autocfg_probe, Some(out_dir)));

        // Normal crate with a build script: --out-dir is deps (or another unit's out dir).
        let normal = remote(&v(BASE), &o);
        assert!(!is_build_script_probe(&normal, Some(out_dir)));
        let cargo1100 = remote(
            &v("--crate-name util src/lib.rs --crate-type lib --emit=dep-info,metadata,link --out-dir E:/w/tgt/debug/build/util/libhash/out"),
            &o,
        );
        assert!(!is_build_script_probe(
            &cargo1100,
            Some(r"E:\w\tgt\debug\build\util\bshash\out")
        ));
    }
}
