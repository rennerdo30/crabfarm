//! Package A. Collect the input files of a job (DESIGN section 8, items 1, 2
//! and 4; the dependency closure, item 3, is `depgraph`, package D) and hash
//! them with a persistent cache.

use crate::proto::{BlobHash, ClientPath};
use std::path::{Path, PathBuf};

pub const MAX_FILE: u64 = 64 * 1024 * 1024;
pub const MAX_TOTAL: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct Collected {
    /// (absolute local path, size, executable)
    pub files: Vec<(PathBuf, u64, bool)>,
    pub total: u64,
}

#[derive(Debug)]
pub enum CollectError {
    TooLarge,
    MissingNativeLib(String),
    Io(std::io::Error),
}

/// Package dir (skip `target/`, `.git/`, dot-dirs, files > MAX_FILE),
/// `OUT_DIR` contents, and native libs for static `-l` libraries (`name.lib`
/// for msvc targets, `lib<name>.a` otherwise) from `native`/`all` search dirs.
/// Exceeding MAX_TOTAL is `TooLarge`.
pub fn collect(
    package_dir: &Path,
    out_dir_env: Option<&Path>,
    search_dirs: &[(String, PathBuf)],
    link_libs: &[(String, String)],
    target_is_msvc: bool,
) -> Result<Collected, CollectError> {
    let _ = (package_dir, out_dir_env, search_dirs, link_libs, target_is_msvc);
    todo!("package A")
}

/// Hash cache keyed by client path, valid while (size, mtime nanos) match.
/// Persisted by the node (postcard file).
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct HashCache {
    pub entries: std::collections::HashMap<ClientPath, (u64, i128, BlobHash)>,
}

impl HashCache {
    /// BLAKE3 of the file (streaming) unless a matching entry exists.
    /// Returns (hash, size).
    pub fn hash(&mut self, client: &ClientPath, path: &Path) -> std::io::Result<(BlobHash, u64)> {
        let _ = (client, path);
        todo!("package A")
    }
}
