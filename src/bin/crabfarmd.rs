//! `crabfarmd`: the crabfarm node without a console window on Windows (the
//! autostart entry and the installer start this one). Behaves like
//! `crabfarm node`; it logs to `<data>/logs/node.log`.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    if let Err(e) = crabfarm::daemon::run_node() {
        // No console here: the log file is the only place a user can look.
        crabfarm::daemon::log_startup_failure(&format!("{e:#}"));
        // A second node is refused, but that is the state the caller wanted
        // (autostart on top of a node started by hand): exit cleanly so a
        // service manager does not restart it in a loop.
        let already_running = crabfarm::config::data_dir()
            .map(|d| crabfarm::daemon::node_running(&d))
            .unwrap_or(false);
        std::process::exit(if already_running { 0 } else { 1 });
    }
}
