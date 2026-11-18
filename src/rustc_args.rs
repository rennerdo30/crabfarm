//! Package A. Parse and classify a rustc command line (DESIGN section 5).

use crate::proto::{Arg, ClientPath, PathStyle};
use std::path::Path;

/// Result of classifying one invocation.
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
pub fn expand_argfiles(args: &[String]) -> std::io::Result<Vec<String>> {
    let _ = args;
    todo!("package A")
}

/// Classify expanded args (without the rustc path).
pub fn classify(args: &[String], opts: &ClassifyOptions) -> Classified {
    let _ = (args, opts);
    todo!("package A")
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

#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn render_maps_every_path_variant() {
        let p = |s: &str| ClientPath::parse(PathStyle::Windows, s).expect("path");
        let args = vec![
            Arg::Plain("--crate-name".into()),
            Arg::Plain("foo".into()),
            Arg::Path(p(r"C:\a\lib.rs")),
            Arg::PrefixedPath { prefix: "--out-dir=".into(), path: p(r"C:\a\out") },
            Arg::PrefixedPath { prefix: "foo=".into(), path: p(r"C:\a\libfoo.rlib") },
            Arg::Plain("--remap-path-prefix".into()),
            Arg::RemapFrom { from: p(r"C:\a"), to: "/x".into() },
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
}
