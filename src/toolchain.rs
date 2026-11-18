//! Package C. Toolchain identity, lookup and installation (DESIGN section 7).

use crate::proto::ToolchainId;
use std::path::{Path, PathBuf};

/// Parse `rustc -vV` output.
pub fn parse_version_verbose(out: &str) -> anyhow::Result<ToolchainId> {
    let _ = out;
    todo!("package C")
}

/// Run `<rustc> -vV` (timeout 30 s) and parse it.
pub fn query(rustc: &Path) -> anyhow::Result<ToolchainId> {
    let _ = rustc;
    todo!("package C")
}

/// An installed toolchain on this machine.
#[derive(Clone, Debug)]
pub struct Installed {
    pub id: ToolchainId,
    /// rustup toolchain name, e.g. `1.100.0-x86_64-pc-windows-msvc`.
    pub name: String,
    /// Direct path to `bin/rustc(.exe)` (not the rustup proxy).
    pub rustc: PathBuf,
    /// Installed targets (`rustup target list --installed --toolchain <name>`).
    pub targets: Vec<String>,
}

/// Executing-peer toolchain registry. Thread-safe; installs run in the
/// background with a 30-minute timeout, child PIDs recorded and only those
/// killed on timeout or shutdown.
pub struct Toolchains {
    // implementation-defined
}

impl Toolchains {
    /// Scan `rustup toolchain list` (+ `rustc -vV` per toolchain, cached on
    /// disk in `<data>/toolchains.toml` by toolchain dir mtime).
    pub fn scan(data_dir: &Path, auto_install: bool) -> anyhow::Result<Toolchains> {
        let _ = (data_dir, auto_install);
        todo!("package C")
    }

    /// Installed toolchains for `Status`.
    pub fn list(&self) -> Vec<Installed> {
        todo!("package C")
    }

    /// Resolve a toolchain that can build `target` for `id` right now.
    /// When missing and auto_install is on, start installing (toolchain
    /// and/or target) in the background and return `None`.
    pub fn resolve(&self, id: &ToolchainId, target: &str) -> Option<Installed> {
        let _ = (id, target);
        todo!("package C")
    }

    /// rustup spec for an id: `release` for stable/beta, `nightly-<commit
    /// date + 1 day>` for nightly; install is verified by commit hash.
    pub fn install_spec(id: &ToolchainId) -> String {
        let _ = id;
        todo!("package C")
    }
}
