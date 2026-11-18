//! `crabfarm status` (DESIGN section 13).

use crate::cargo_cmd::query_status;
use crate::config;
use crate::proto::{Message, PeerInfo, Status};

fn counter(counters: &[(String, u64)], name: &str) -> u64 {
    counters
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| *v)
        .unwrap_or(0)
}

fn ago(now: u64, then: u64) -> String {
    let s = now.saturating_sub(then);
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        _ => format!("{}h ago", s / 3600),
    }
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// The table, first line is the answer to "is my farm usable".
pub fn format_status(
    local: &Status,
    peers: &[(PeerInfo, Option<Status>)],
    counters: &[(String, u64)],
    now: u64,
) -> String {
    let online: Vec<&(PeerInfo, Option<Status>)> =
        peers.iter().filter(|(_, s)| s.is_some()).collect();
    let free: u32 = online
        .iter()
        .filter_map(|(_, s)| s.as_ref())
        .filter(|s| s.accepting)
        .map(|s| s.slots_total.saturating_sub(s.slots_busy))
        .sum();
    let mut out = String::new();
    out.push_str(&format!(
        "farm: {} of {} peers online, {} free remote slots\n",
        online.len(),
        peers.len(),
        free
    ));
    out.push_str(&format!(
        "this node: {} ({}), serving {}/{}, local {}/{} running\n\n",
        local.node_name,
        local.host_triple,
        local.slots_busy,
        local.slots_total,
        counter(counters, "running_local"),
        counter(counters, "local_slots"),
    ));
    if !peers.is_empty() {
        let w = peers
            .iter()
            .map(|(p, _)| p.node_name.chars().count())
            .max()
            .unwrap_or(4)
            .max(4);
        out.push_str(&format!(
            "{:<w$}  {:<28}  {:<7}  {}\n",
            "PEER",
            "HOST",
            "SLOTS",
            "LAST SEEN",
            w = w
        ));
        for (p, s) in peers {
            match s {
                Some(s) => out.push_str(&format!(
                    "{:<w$}  {:<28}  {:<7}  {}\n",
                    p.node_name,
                    s.host_triple,
                    format!("{}/{}", s.slots_busy, s.slots_total),
                    ago(now, p.last_seen),
                    w = w
                )),
                None => out.push_str(&format!(
                    "{:<w$}  {:<28}  {:<7}  {}\n",
                    p.node_name,
                    "-",
                    "offline",
                    ago(now, p.last_seen),
                    w = w
                )),
            }
        }
        out.push('\n');
    }
    let local_total: u64 = counters
        .iter()
        .filter(|(k, _)| k.starts_with("local."))
        .map(|(_, v)| *v)
        .sum();
    out.push_str(&format!(
        "jobs: {} remote ok, {} remote failed, {} local, {} running remote\n",
        counter(counters, "remote_ok"),
        counter(counters, "remote_failed") + counter(counters, "remote_exit_nonzero"),
        local_total,
        counter(counters, "running_remote"),
    ));
    let reasons: Vec<String> = counters
        .iter()
        .filter_map(|(k, v)| k.strip_prefix("local.").map(|r| format!("{r} {v}")))
        .collect();
    if !reasons.is_empty() {
        out.push_str(&format!("local because: {}\n", reasons.join(", ")));
    }
    out.push_str(&format!(
        "pipelining: {} artifact notices relayed before their job ended\n",
        counter(counters, "early_notices"),
    ));
    out.push_str(&cache_line(counters));
    out.push_str(&format!(
        "traffic: {} up, {} down, {} jobs served for peers\n",
        mib(counter(counters, "bytes_up")),
        mib(counter(counters, "bytes_down")),
        counter(counters, "served")
    ));
    out
}

/// The compile cache line of `crabfarm status` and `crabfarm cache stats`
/// (DESIGN section 16): hits (this node / the farm), misses, output reused.
pub fn cache_line(counters: &[(String, u64)]) -> String {
    if counter(counters, "cache_enabled") == 0 {
        return "compile cache: off
"
        .to_string();
    }
    let local = counter(counters, "cache_hit_local");
    let farm = counter(counters, "cache_hit_farm");
    format!(
        "compile cache: {} hits ({} local, {} farm), {} misses, {} of rustc output reused, {} stored, {} served to peers, {} unusable
",
        local + farm,
        local,
        farm,
        counter(counters, "cache_miss"),
        mib(counter(counters, "cache_bytes_saved")),
        counter(counters, "cache_stored"),
        counter(counters, "cache_served"),
        counter(counters, "cache_corrupt") + counter(counters, "cache_error"),
    )
}

/// `crabfarm cache stats`: what is on disk, and the node's counters when it runs.
pub fn cache_stats() -> i32 {
    let Ok(data_dir) = config::data_dir() else {
        eprintln!("crabfarm: cannot find the data directory");
        return 1;
    };
    let cfg = config::Config::load(&data_dir).unwrap_or_default();
    let u = crate::ccache::disk_usage(&data_dir);
    println!(
        "{} entries, {} files, {} on disk (limit {} GB, config `cache = {}`)",
        u.entries,
        u.blobs,
        mib(u.bytes),
        cfg.cache_entries_max_gb,
        cfg.cache
    );
    match query_status(&data_dir) {
        Some(Message::FarmStatus { counters, .. }) => print!("{}", cache_line(&counters)),
        _ => println!("The node is not running, so there are no hit counters."),
    }
    0
}

/// `crabfarm cache clear`: delete every entry and file.
pub fn cache_clear() -> i32 {
    let Ok(data_dir) = config::data_dir() else {
        eprintln!("crabfarm: cannot find the data directory");
        return 1;
    };
    match crate::ccache::clear(&data_dir) {
        Ok(u) => {
            println!(
                "Removed {} entries ({} files, {}).",
                u.entries,
                u.blobs,
                mib(u.bytes)
            );
            0
        }
        Err(e) => {
            eprintln!("crabfarm: {e:#}");
            1
        }
    }
}

/// The line shown while wrappers skip the node because one of them found it
/// silent (CF-25), `None` when there is no such finding.
pub fn unresponsive_note(until: Option<u64>, now: u64) -> Option<String> {
    let left = until?.checked_sub(now).filter(|s| *s > 0)?;
    Some(format!(
        "warning: the node did not answer a wrapper recently; rustc runs locally without asking it for {left} more seconds (a node that answers removes this)\n"
    ))
}

pub fn run() -> i32 {
    let Ok(data_dir) = config::data_dir() else {
        eprintln!("crabfarm: cannot find the data directory");
        return 1;
    };
    let now = crate::node::unix_now();
    let note = unresponsive_note(crate::ipc::unresponsive_until(&data_dir), now);
    match query_status(&data_dir) {
        Some(Message::FarmStatus {
            local,
            peers,
            counters,
        }) => {
            if let Some(n) = &note {
                print!("{n}");
            }
            print!("{}", format_status(&local, &peers, &counters, now));
            0
        }
        _ => {
            if let Some(n) = &note {
                eprint!("{n}");
            }
            eprintln!("crabfarm: the node is not running. Start it with `crabfarm node`.");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::NodeId;

    fn st(name: &str, total: u32, busy: u32) -> Status {
        Status {
            node_id: NodeId([1; 16]),
            node_name: name.into(),
            host_triple: "x86_64-pc-windows-msvc".into(),
            slots_total: total,
            slots_busy: busy,
            queue_len: 0,
            speed: 100,
            toolchains: vec![],
            targets: vec![],
            accepting: true,
        }
    }

    #[test]
    fn the_cache_line_shows_hits_misses_and_savings_or_that_it_is_off() {
        assert_eq!(
            cache_line(&[]),
            "compile cache: off
"
        );
        let c = |k: &str, v: u64| (k.to_string(), v);
        let line = cache_line(&[
            c("cache_enabled", 1),
            c("cache_hit_local", 5),
            c("cache_hit_farm", 2),
            c("cache_miss", 3),
            c("cache_bytes_saved", 4 * 1024 * 1024),
        ]);
        assert!(
            line.contains("7 hits (5 local, 2 farm), 3 misses, 4.0 MiB"),
            "{line}"
        );
    }

    #[test]
    fn status_says_when_wrappers_skip_the_node() {
        assert_eq!(unresponsive_note(None, 100), None);
        assert_eq!(unresponsive_note(Some(100), 100), None, "expired");
        let line = unresponsive_note(Some(160), 100).expect("marker is live");
        assert!(line.contains("did not answer") && line.contains("60 more seconds"));
    }

    #[test]
    fn table_shows_peers_counters_and_offline_nodes() {
        let info = |n: &str, seen: u64| PeerInfo {
            node_id: NodeId([2; 16]),
            node_name: n.into(),
            addrs: vec![],
            last_seen: seen,
        };
        let peers = vec![
            (info("desktop", 995), Some(st("desktop", 16, 4))),
            (info("laptop", 100), None),
        ];
        let counters = vec![
            ("remote_ok".to_string(), 12),
            ("local.NoPeer".to_string(), 3),
            ("bytes_up".to_string(), 3 * 1024 * 1024),
            ("local_slots".to_string(), 8),
        ];
        let t = format_status(&st("me", 8, 0), &peers, &counters, 1000);
        assert!(t.starts_with("farm: 1 of 2 peers online, 12 free remote slots"));
        assert!(t.contains("desktop") && t.contains("4/16") && t.contains("5s ago"));
        assert!(t.contains("laptop") && t.contains("offline"));
        assert!(t.contains("12 remote ok") && t.contains("3 local") && t.contains("NoPeer 3"));
        assert!(t.contains("3.0 MiB up"));
    }
}
