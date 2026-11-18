//! `crabfarm status` (DESIGN section 13).

use crate::cargo_cmd::query_status;
use crate::config;
use crate::proto::{Message, PeerInfo, Status};

fn counter(counters: &[(String, u64)], name: &str) -> u64 {
    counters.iter().find(|(k, _)| k == name).map(|(_, v)| *v).unwrap_or(0)
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
pub fn format_status(local: &Status, peers: &[(PeerInfo, Option<Status>)], counters: &[(String, u64)], now: u64) -> String {
    let online: Vec<&(PeerInfo, Option<Status>)> = peers.iter().filter(|(_, s)| s.is_some()).collect();
    let free: u32 = online.iter().filter_map(|(_, s)| s.as_ref()).filter(|s| s.accepting).map(|s| s.slots_total.saturating_sub(s.slots_busy)).sum();
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
        let w = peers.iter().map(|(p, _)| p.node_name.chars().count()).max().unwrap_or(4).max(4);
        out.push_str(&format!("{:<w$}  {:<28}  {:<7}  {}\n", "PEER", "HOST", "SLOTS", "LAST SEEN", w = w));
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
                None => out.push_str(&format!("{:<w$}  {:<28}  {:<7}  {}\n", p.node_name, "-", "offline", ago(now, p.last_seen), w = w)),
            }
        }
        out.push('\n');
    }
    let local_total: u64 = counters.iter().filter(|(k, _)| k.starts_with("local.")).map(|(_, v)| *v).sum();
    out.push_str(&format!(
        "jobs: {} remote ok, {} remote failed, {} local, {} running remote\n",
        counter(counters, "remote_ok"),
        counter(counters, "remote_failed") + counter(counters, "remote_exit_nonzero"),
        local_total,
        counter(counters, "running_remote"),
    ));
    let reasons: Vec<String> =
        counters.iter().filter_map(|(k, v)| k.strip_prefix("local.").map(|r| format!("{r} {v}"))).collect();
    if !reasons.is_empty() {
        out.push_str(&format!("local because: {}\n", reasons.join(", ")));
    }
    out.push_str(&format!(
        "traffic: {} up, {} down, {} jobs served for peers\n",
        mib(counter(counters, "bytes_up")),
        mib(counter(counters, "bytes_down")),
        counter(counters, "served")
    ));
    out
}

pub fn run() -> i32 {
    let Ok(data_dir) = config::data_dir() else {
        eprintln!("crabfarm: cannot find the data directory");
        return 1;
    };
    match query_status(&data_dir) {
        Some(Message::FarmStatus { local, peers, counters }) => {
            let now = crate::node::unix_now();
            print!("{}", format_status(&local, &peers, &counters, now));
            0
        }
        _ => {
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
    fn table_shows_peers_counters_and_offline_nodes() {
        let info = |n: &str, seen: u64| PeerInfo { node_id: NodeId([2; 16]), node_name: n.into(), addrs: vec![], last_seen: seen };
        let peers = vec![(info("desktop", 995), Some(st("desktop", 16, 4))), (info("laptop", 100), None)];
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
