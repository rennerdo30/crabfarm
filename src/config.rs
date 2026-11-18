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
    pub remote_incremental: RemoteIncremental,
    pub local_only: Vec<String>,
    pub sensitive: Vec<String>,
    pub retain_blobs: bool,
    pub cache_max_gb: u64,
    pub job_timeout_min: u64,
    pub forward_env: Vec<String>,
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
            slots: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4),
            accept_remote_jobs: true,
            peers: Vec::new(),
            auto_install: true,
            remote_incremental: RemoteIncremental::Local,
            local_only: Vec::new(),
            sensitive: Vec::new(),
            retain_blobs: true,
            cache_max_gb: 20,
            job_timeout_min: 30,
            forward_env: Vec::new(),
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
