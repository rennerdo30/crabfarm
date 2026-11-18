//! `crabfarm cargo <args>`: cargo with crabfarm as `RUSTC_WRAPPER` and a
//! farm-sized `-j` (DESIGN sections 2 and 4).

use crate::config::{self, Config};
use crate::ipc;
use crate::proto::Message;
use std::process::Command;
use std::time::Duration;

/// `-j` cap (DESIGN section 4).
pub const MAX_JOBS: u32 = 128;

/// Subcommands that accept `-j`; anything else gets no extra flag.
const JOBS_COMMANDS: [&str; 19] = [
    "build", "b", "check", "c", "clippy", "test", "t", "run", "r", "bench", "doc", "d", "install",
    "rustc", "rustdoc", "fix", "package", "publish", "miri",
];

/// Global options that take their value as the next argument.
const GLOBAL_WITH_VALUE: [&str; 4] = ["--config", "-Z", "-C", "--color"];

/// Index of the cargo subcommand (`+toolchain` and global options skipped).
pub fn subcommand_index(args: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a.starts_with('+') {
            i += 1;
        } else if a.starts_with('-') {
            i += if GLOBAL_WITH_VALUE.contains(&a) { 2 } else { 1 };
        } else {
            return Some(i);
        }
    }
    None
}

/// True when the user already chose a job count (before any `--`).
pub fn has_jobs_flag(args: &[String]) -> bool {
    for a in args {
        if a == "--" {
            return false;
        }
        if a == "-j" || a == "--jobs" || a.starts_with("--jobs=") {
            return true;
        }
        if let Some(rest) = a.strip_prefix("-j")
            && rest.starts_with(|c: char| c.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

/// Insert `-j <jobs>` right after the subcommand (`build -j 24 --release`).
/// Arguments are returned unchanged when the user gave their own `-j`, the
/// subcommand does not take one, or there is no subcommand.
pub fn insert_jobs(args: &[String], jobs: u32) -> Vec<String> {
    let Some(i) = subcommand_index(args) else {
        return args.to_vec();
    };
    if !JOBS_COMMANDS.contains(&args[i].as_str()) || has_jobs_flag(&args[i + 1..]) {
        return args.to_vec();
    }
    let mut out = args.to_vec();
    out.insert(i + 1, "-j".to_string());
    out.insert(i + 2, jobs.to_string());
    out
}

/// Local slots plus the free slots of every peer that reported a status.
pub fn farm_jobs(
    local_slots: u32,
    peers: &[(crate::proto::PeerInfo, Option<crate::proto::Status>)],
) -> u32 {
    let remote: u32 = peers
        .iter()
        .filter_map(|(_, s)| s.as_ref())
        .filter(|s| s.accepting)
        .map(|s| s.slots_total.saturating_sub(s.slots_busy))
        .sum();
    local_slots.saturating_add(remote).clamp(1, MAX_JOBS)
}

/// Ask the node for its status; `None` when it is not running.
pub fn query_status(data_dir: &std::path::Path) -> Option<Message> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        let mut conn = ipc::connect(data_dir).await.ok()?;
        conn.send(&Message::QueryStatus).await.ok()?;
        match tokio::time::timeout(Duration::from_secs(3), conn.recv()).await {
            Ok(Ok(Some(m @ Message::FarmStatus { .. }))) => Some(m),
            _ => None,
        }
    })
}

pub fn run(args: &[String]) -> i32 {
    let Ok(data_dir) = config::data_dir() else {
        eprintln!("crabfarm: cannot find the data directory");
        return 1;
    };
    let cfg = Config::load(&data_dir).unwrap_or_default();
    let mut cargo_args = args.to_vec();
    match query_status(&data_dir) {
        Some(Message::FarmStatus { peers, .. }) => {
            cargo_args = insert_jobs(args, farm_jobs(cfg.local_slots, &peers));
        }
        _ => {
            eprintln!(
                "crabfarm: node is not running, building locally. Start it with `crabfarm node` in another terminal."
            );
        }
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("crabfarm: cannot find my own executable: {e}");
            return 1;
        }
    };
    match Command::new("cargo")
        .args(&cargo_args)
        .env("RUSTC_WRAPPER", &exe)
        .status()
    {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("crabfarm: could not run cargo: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{NodeId, PeerInfo, Status};

    fn a(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn jobs_go_right_after_the_subcommand() {
        assert_eq!(
            insert_jobs(&a(&["build", "--release"]), 24),
            a(&["build", "-j", "24", "--release"])
        );
        assert_eq!(
            insert_jobs(&a(&["+nightly", "test", "-p", "x"]), 8),
            a(&["+nightly", "test", "-j", "8", "-p", "x"])
        );
        assert_eq!(
            insert_jobs(&a(&["--config", "a=b", "check"]), 8),
            a(&["--config", "a=b", "check", "-j", "8"])
        );
        assert_eq!(
            insert_jobs(&a(&["-Z", "unstable", "build"]), 3),
            a(&["-Z", "unstable", "build", "-j", "3"])
        );
        assert_eq!(
            insert_jobs(&a(&["run", "--", "-j", "2"]), 9),
            a(&["run", "-j", "9", "--", "-j", "2"])
        );
    }

    #[test]
    fn user_jobs_and_other_commands_are_left_alone() {
        for args in [
            a(&["build", "-j", "4"]),
            a(&["build", "--jobs=4"]),
            a(&["build", "-j4"]),
            a(&["build", "--jobs", "4"]),
        ] {
            assert_eq!(insert_jobs(&args, 24), args);
        }
        assert_eq!(insert_jobs(&a(&["fmt"]), 24), a(&["fmt"]));
        assert_eq!(insert_jobs(&a(&["--version"]), 24), a(&["--version"]));
        assert_eq!(insert_jobs(&[], 24), Vec::<String>::new());
    }

    fn peer(total: u32, busy: u32, accepting: bool) -> (PeerInfo, Option<Status>) {
        (
            PeerInfo {
                node_id: NodeId([1; 16]),
                node_name: "p".into(),
                addrs: vec![],
                last_seen: 0,
            },
            Some(Status {
                node_id: NodeId([1; 16]),
                node_name: "p".into(),
                host_triple: "h".into(),
                slots_total: total,
                slots_busy: busy,
                queue_len: 0,
                speed: 100,
                toolchains: vec![],
                targets: vec![],
                accepting,
            }),
        )
    }

    #[test]
    fn farm_jobs_adds_free_remote_slots_and_caps() {
        assert_eq!(farm_jobs(4, &[peer(8, 3, true), peer(16, 0, false)]), 9);
        assert_eq!(farm_jobs(4, &[(peer(8, 0, true).0, None)]), 4);
        assert_eq!(farm_jobs(100, &[peer(64, 0, true)]), MAX_JOBS);
        assert_eq!(farm_jobs(0, &[]), 1);
    }
}
