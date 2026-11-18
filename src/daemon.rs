//! Running the node as a process: shared by `crabfarm node` and `crabfarmd`
//! (the console-less binary the autostart and the installer use), plus the
//! clean stop (`crabfarm node --stop`).

use crate::proto::Message;
use crate::{config, ipc, node};
use anyhow::Context;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long `--stop` waits for the node to let go of its data directory.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(20);

/// Run the node in this process until Ctrl-C or a stop request. A second node
/// on the same data directory is refused before anything else is touched.
pub fn run_node() -> anyhow::Result<()> {
    let data_dir = config::data_dir()?;
    // First, before logging, the job sweep or any port: a second node on
    // the same directory must not touch anything of the first (CF-19).
    let lock = node::NodeLock::acquire(&data_dir)?;
    node::init_logging(&data_dir);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("start async runtime")?;
    rt.block_on(async {
        let token = tokio_util::sync::CancellationToken::new();
        let t2 = token.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            t2.cancel();
        });
        node::run(data_dir, token, lock).await
    })
}

/// Best effort: tell the user why the node did not start, in the node log
/// (a console-less process has no other place) and on stderr.
pub fn log_startup_failure(message: &str) {
    use std::io::Write;
    eprintln!("crabfarmd: {message}");
    let Ok(data_dir) = config::data_dir() else {
        return;
    };
    let logs = data_dir.join("logs");
    if std::fs::create_dir_all(&logs).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join("node.log"))
    {
        let _ = writeln!(f, "crabfarmd: could not start: {message}");
    }
}

/// True when a node holds the data directory's lock. A probe only: it never
/// creates the directory or the lock file.
pub fn node_running(data_dir: &Path) -> bool {
    node::NodeLock::is_held(data_dir)
}

/// Ask the node of `data_dir` to shut down and wait until it has released the
/// directory. `Ok(false)` means no node was running.
pub fn stop_node(data_dir: &Path, timeout: Duration) -> anyhow::Result<bool> {
    if !node_running(data_dir) {
        return Ok(false);
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("start async runtime")?;
    rt.block_on(async {
        let mut conn = ipc::connect(data_dir)
            .await
            .context("a node holds the data directory but does not answer on local IPC")?;
        conn.send(&Message::Shutdown).await?;
        anyhow::Ok(())
    })?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !node_running(data_dir) {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    anyhow::bail!(
        "the node was asked to stop but is still running after {} s",
        timeout.as_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_node_means_not_running_and_stop_is_a_noop() {
        let d = tempfile::tempdir().unwrap();
        assert!(!node_running(d.path()));
        assert!(!stop_node(d.path(), Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn the_probe_creates_nothing() {
        let d = tempfile::tempdir().unwrap();
        let missing = d.path().join("not-yet/data");
        assert!(!node_running(&missing));
        assert!(!missing.exists());
        let empty = d.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(!node_running(&empty));
        assert!(!empty.join("node.lock").exists());
    }

    #[test]
    fn a_released_lock_is_not_running() {
        let d = tempfile::tempdir().unwrap();
        drop(node::NodeLock::acquire(d.path()).unwrap());
        assert!(!node_running(d.path()));
    }

    #[test]
    fn a_held_lock_counts_as_running() {
        let d = tempfile::tempdir().unwrap();
        let _lock = node::NodeLock::acquire(d.path()).unwrap();
        assert!(node_running(d.path()));
    }
}
