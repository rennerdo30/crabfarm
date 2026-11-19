//! Node configuration and data directory (DESIGN sections 3 and 12).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub node_name: String,
    pub listen: String,
    pub slots: u32,
    pub accept_remote_jobs: bool,
    pub peers: Vec<String>,
    pub auto_install: bool,
    /// Most toolchains this node installs on its own for peers (DESIGN section 7).
    pub max_auto_toolchains: usize,
    pub remote_incremental: RemoteIncremental,
    pub local_only: Vec<String>,
    pub sensitive: Vec<String>,
    pub retain_blobs: bool,
    pub cache_max_gb: u64,
    /// The compile cache (DESIGN section 16, CF-28): store every successful
    /// eligible rustc result and reuse it here and on peers.
    pub cache: bool,
    /// Size limit of the compile cache's own store, in GB (LRU eviction).
    pub cache_entries_max_gb: u64,
    pub job_timeout_min: u64,
    pub forward_env: Vec<String>,
    /// Variable names this node accepts from submitters beyond cargo's own
    /// (their `forward_env`); empty by default (DESIGN sections 9 and 12, CF-19).
    pub accept_env: Vec<String>,
    /// Accept the variable names a submitter's build script declared with
    /// `rustc-env` (`JobSpec.build_env`). Default true: a submitter's build
    /// script already runs code on its own machine and the members of a farm
    /// trust each other (SECURITY.md); false makes every job that declares one
    /// run on its submitter (CF-25).
    pub accept_build_env: bool,
    /// Concurrent local rustc runs on this machine (default logical CPUs).
    pub local_slots: u32,
    /// Announce and browse via mDNS (tests set false).
    pub mdns: bool,
    /// Send eligible jobs to a peer with a free slot even when local slots are free.
    pub prefer_remote: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteIncremental {
    Local,
    Strip,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            node_name: hostname(),
            listen: format!("0.0.0.0:{}", crate::proto::DEFAULT_PORT),
            slots: std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(4),
            accept_remote_jobs: true,
            peers: Vec::new(),
            auto_install: true,
            max_auto_toolchains: crate::toolchain::DEFAULT_MAX_AUTO_TOOLCHAINS,
            remote_incremental: RemoteIncremental::Local,
            local_only: Vec::new(),
            sensitive: Vec::new(),
            retain_blobs: true,
            cache_max_gb: 20,
            cache: true,
            cache_entries_max_gb: 10,
            job_timeout_min: 30,
            forward_env: Vec::new(),
            accept_env: Vec::new(),
            accept_build_env: true,
            local_slots: std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(4),
            mdns: true,
            prefer_remote: false,
        }
    }
}

impl Config {
    /// Load `<data>/config.toml`; defaults when the file does not exist.
    pub fn load(data_dir: &Path) -> anyhow::Result<Config> {
        let path = data_dir.join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(toml::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Data directory: `CRABFARM_HOME` if set (tests, multiple nodes on one
/// machine), else the platform data dir (`%APPDATA%\renner\crabfarm\data`,
/// `~/Library/Application Support/dev.renner.crabfarm`, `~/.local/share/crabfarm`).
pub fn data_dir() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("CRABFARM_HOME") {
        return Ok(PathBuf::from(p));
    }
    let dirs = directories::ProjectDirs::from("dev", "renner", "crabfarm")
        .ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    Ok(dirs.data_dir().to_path_buf())
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "node".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compile_cache_is_on_by_default_with_ten_gb() {
        let c = Config::default();
        assert!(c.cache);
        assert_eq!(c.cache_entries_max_gb, 10);
        let off: Config = toml::from_str(
            "cache = false
cache_entries_max_gb = 2",
        )
        .unwrap();
        assert!(!off.cache);
        assert_eq!(off.cache_entries_max_gb, 2);
    }

    /// CF-25: build-script variables are accepted unless the operator opts out.
    #[test]
    fn accept_build_env_defaults_to_true_and_can_be_turned_off() {
        assert!(Config::default().accept_build_env);
        let on: Config = toml::from_str("node_name = \"n\"").unwrap();
        assert!(on.accept_build_env);
        let off: Config = toml::from_str("accept_build_env = false").unwrap();
        assert!(!off.accept_build_env);
    }
}
