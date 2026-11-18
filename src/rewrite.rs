//! Package A. Rewrite rustc output from sandbox paths to client paths
//! (DESIGN section 6).

/// Ordered `(sandbox prefix, client prefix)` pairs, longest sandbox prefix
/// first. Built from `SandboxMap::remaps`.
#[derive(Clone, Debug, Default)]
pub struct Rewriter {
    pub pairs: Vec<(String, String)>,
    /// Client is Windows: convert `/` to `\` inside a rewritten path token.
    pub client_windows: bool,
}

impl Rewriter {
    /// Rewrite one stderr line. JSON lines (start with `{`) are parsed with
    /// serde_json and every string value is rewritten (including the
    /// `rendered` field), then re-serialized compactly; other lines are
    /// rewritten as text. Unparseable JSON falls back to text rewriting.
    pub fn stderr_line(&self, line: &str) -> String {
        let _ = line;
        todo!("package A")
    }

    /// Rewrite a Makefile-style dep-info file (`.d`), preserving `\ ` escapes
    /// and `# env-dep:` lines.
    pub fn dep_info(&self, contents: &str) -> String {
        let _ = contents;
        todo!("package A")
    }

    /// Rewrite every occurrence of a sandbox prefix in a string.
    pub fn text(&self, s: &str) -> String {
        let _ = s;
        todo!("package A")
    }
}

/// If `line` is a rustc artifact notification
/// (`{"artifact":"<path>","emit":"<kind>"}`), return `(path, emit)`.
pub fn artifact(line: &str) -> Option<(String, String)> {
    let _ = line;
    todo!("package A")
}

/// Names from `# env-dep:NAME` / `# env-dep:NAME=VALUE` lines of a dep-info file.
pub fn env_deps(dep_info: &str) -> Vec<String> {
    let _ = dep_info;
    todo!("package A")
}
