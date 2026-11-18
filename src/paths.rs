//! Package A. Client paths and sandbox mapping (DESIGN section 6).
//!
//! Pure functions, no I/O. Normalization is lexical (no canonicalize), so it
//! works for paths that do not exist on the executing peer.

use crate::proto::{ClientPath, PathStyle};
use std::path::{Path, PathBuf};

impl ClientPath {
    /// Parse and normalize an absolute path string in the given style.
    /// Windows: uppercase drive letter, strip `\\?\` (and `\\?\UNC\`), accept
    /// `/` as separator, collapse `.`/`..` lexically, no trailing separator
    /// (except a bare root like `C:\`). Returns `None` for relative paths.
    pub fn parse(style: PathStyle, s: &str) -> Option<ClientPath> {
        let _ = (style, s);
        todo!("package A")
    }

    /// Absolute path on this machine (style = this OS). Relative paths are
    /// joined to `cwd` first.
    pub fn from_local(p: &Path, cwd: &Path) -> Option<ClientPath> {
        let _ = (p, cwd);
        todo!("package A")
    }

    /// This path as a local `PathBuf` (only meaningful on the submitting machine).
    pub fn to_local(&self) -> PathBuf {
        todo!("package A")
    }

    /// True if `self` equals `prefix` or lies under it (component-wise;
    /// case-insensitive for Windows style).
    pub fn starts_with(&self, prefix: &ClientPath) -> bool {
        let _ = prefix;
        todo!("package A")
    }

    /// The root this path belongs to: `C:\` for Windows drive paths,
    /// `\\server\share` for UNC, `/` for Posix.
    pub fn root(&self) -> ClientPath {
        todo!("package A")
    }
}

/// Maps client paths into a job sandbox root on the executing peer and back.
#[derive(Clone, Debug)]
pub struct SandboxMap {
    pub root: PathBuf,
}

impl SandboxMap {
    pub fn new(root: PathBuf) -> Self {
        SandboxMap { root }
    }

    /// `C:\a\b` -> `<root>/C/a/b`, `\\srv\share\a` -> `<root>/UNC/srv/share/a`,
    /// `/a/b` -> `<root>/_/a/b`.
    pub fn to_sandbox(&self, p: &ClientPath) -> PathBuf {
        let _ = p;
        todo!("package A")
    }

    /// Inverse of `to_sandbox`; `None` if `p` is not under `root`.
    pub fn from_sandbox(&self, p: &Path, style: PathStyle) -> Option<ClientPath> {
        let _ = (p, style);
        todo!("package A")
    }

    /// `(sandbox root string, client root string)` for every distinct root of
    /// `paths`, for `--remap-path-prefix` and output rewriting. The sandbox
    /// side is rendered as rustc on this host will print it.
    pub fn remaps<'a>(&self, paths: impl IntoIterator<Item = &'a ClientPath>) -> Vec<(String, String)> {
        let _ = paths.into_iter();
        todo!("package A")
    }
}
