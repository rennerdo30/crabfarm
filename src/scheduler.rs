//! Job placement (DESIGN section 4). Pure functions: the node feeds them the
//! peer statuses and acts on the answer.
//!
//! P1 simplification: `speed` is the constant [`REFERENCE_SPEED`] for every
//! node, so a candidate's score is its free slots times 100 minus the link
//! penalty. One second of upload costs as much as one free slot.

use crate::proto::{Status, ToolchainId};

/// Relative per-slot throughput of every node in P1 (no benchmark yet).
pub const REFERENCE_SPEED: u32 = 100;
/// Bandwidth assumed until a transfer to the peer was measured (100 Mbit/s).
pub const DEFAULT_BANDWIDTH_BPS: u64 = 100_000_000;

/// What a job needs from the executing peer.
pub struct JobNeeds<'a> {
    pub toolchain: &'a ToolchainId,
    pub target: &'a str,
    /// Proc-macro jobs: the peer must run the same host triple (until P3).
    pub needs_host: bool,
    pub local_host: &'a str,
}

/// Can this peer take the job right now? (Free slots are checked separately.)
pub fn eligible(status: &Status, needs: &JobNeeds) -> bool {
    if !status.accepting {
        return false;
    }
    if needs.needs_host && status.host_triple != needs.local_host {
        return false;
    }
    status
        .toolchains
        .iter()
        .zip(&status.targets)
        .any(|(t, targets)| t.same_compiler(needs.toolchain) && targets.iter().any(|x| x == needs.target))
}

/// Score points lost for uploading `upload_bytes`: one reference slot per second.
pub fn link_penalty(upload_bytes: u64, bandwidth_bps: u64) -> i64 {
    let bw = bandwidth_bps.max(1) as u128;
    let millis = (upload_bytes as u128 * 8 * 1000) / bw;
    (millis as i64).saturating_mul(REFERENCE_SPEED as i64) / 1000
}

pub fn score(free_slots: u32, speed: u32, penalty: i64) -> i64 {
    free_slots as i64 * speed as i64 - penalty
}

/// A peer that passed `eligible`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub free_slots: u32,
    pub speed: u32,
    pub upload_bytes: u64,
    pub bandwidth_bps: u64,
}

impl Candidate {
    pub fn score(&self) -> i64 {
        score(self.free_slots, self.speed, link_penalty(self.upload_bytes, self.bandwidth_bps))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Choice {
    Local,
    /// Index into the candidate list.
    Peer(usize),
}

/// Pick where a job runs. Local is a candidate with link cost 0 while it has a
/// free slot; peers without a free slot are never chosen; ties go local.
/// `prefer_remote` picks the best peer with a free slot whenever one exists.
pub fn choose(local_free: u32, prefer_remote: bool, cands: &[Candidate]) -> Choice {
    let best_peer = cands
        .iter()
        .enumerate()
        .filter(|(_, c)| c.free_slots > 0)
        .max_by_key(|(i, c)| (c.score(), std::cmp::Reverse(*i)))
        .map(|(i, c)| (i, c.score()));
    match best_peer {
        None => Choice::Local,
        Some((i, _)) if prefer_remote => Choice::Peer(i),
        Some((i, s)) => {
            if local_free > 0 && score(local_free, REFERENCE_SPEED, 0) >= s {
                Choice::Local
            } else {
                Choice::Peer(i)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tc(commit: &str) -> ToolchainId {
        ToolchainId { release: "1.100.0".into(), commit_hash: commit.into(), commit_date: "2026-11-09".into(), host: "x86_64-pc-windows-msvc".into() }
    }

    fn status(host: &str, toolchains: Vec<ToolchainId>, targets: Vec<Vec<String>>, accepting: bool) -> Status {
        Status {
            node_id: crate::proto::NodeId([1; 16]),
            node_name: "p".into(),
            host_triple: host.into(),
            slots_total: 8,
            slots_busy: 0,
            queue_len: 0,
            speed: REFERENCE_SPEED,
            toolchains,
            targets,
            accepting,
        }
    }

    fn cand(free: u32, upload: u64) -> Candidate {
        Candidate { free_slots: free, speed: REFERENCE_SPEED, upload_bytes: upload, bandwidth_bps: DEFAULT_BANDWIDTH_BPS }
    }

    #[test]
    fn peer_with_more_free_slots_beats_a_nearly_full_local() {
        assert_eq!(choose(1, false, &[cand(8, 0)]), Choice::Peer(0));
    }

    #[test]
    fn tie_goes_local() {
        assert_eq!(choose(4, false, &[cand(4, 0)]), Choice::Local);
    }

    #[test]
    fn upload_cost_can_flip_the_choice() {
        // 2 free slots on the peer would win 200 to 100, but 50 MB at 100 Mbit/s
        // is 4 s = 400 points of penalty.
        assert_eq!(choose(1, false, &[cand(2, 0)]), Choice::Peer(0));
        assert_eq!(choose(1, false, &[cand(2, 50_000_000)]), Choice::Local);
    }

    #[test]
    fn full_peers_are_never_chosen_and_local_is_the_fallback() {
        assert_eq!(choose(0, false, &[cand(0, 0)]), Choice::Local);
        assert_eq!(choose(0, true, &[cand(0, 0)]), Choice::Local);
    }

    #[test]
    fn prefer_remote_ignores_free_local_slots() {
        assert_eq!(choose(32, true, &[cand(1, 0)]), Choice::Peer(0));
        assert_eq!(choose(32, false, &[cand(1, 0)]), Choice::Local);
    }

    #[test]
    fn best_peer_wins_and_equal_peers_pick_the_first() {
        assert_eq!(choose(0, false, &[cand(2, 0), cand(5, 0), cand(5, 0)]), Choice::Peer(1));
    }

    #[test]
    fn penalty_scales_with_bytes_and_bandwidth() {
        assert_eq!(link_penalty(0, DEFAULT_BANDWIDTH_BPS), 0);
        // 12.5 MB = 100 Mbit = 1 s = 100 points.
        assert_eq!(link_penalty(12_500_000, DEFAULT_BANDWIDTH_BPS), 100);
        assert_eq!(link_penalty(12_500_000, 1_000_000_000), 10);
    }

    #[test]
    fn eligibility_checks_toolchain_target_host_and_accepting() {
        let id = tc("abc");
        let needs = JobNeeds { toolchain: &id, target: "x86_64-pc-windows-msvc", needs_host: false, local_host: "x86_64-pc-windows-msvc" };
        let ok = status("x86_64-pc-windows-msvc", vec![tc("abc")], vec![vec!["x86_64-pc-windows-msvc".into()]], true);
        assert!(eligible(&ok, &needs));
        // Other compiler build.
        assert!(!eligible(&status("x86_64-pc-windows-msvc", vec![tc("def")], vec![vec!["x86_64-pc-windows-msvc".into()]], true), &needs));
        // Target missing.
        assert!(!eligible(&status("x86_64-pc-windows-msvc", vec![tc("abc")], vec![vec!["aarch64-apple-darwin".into()]], true), &needs));
        // Not accepting.
        assert!(!eligible(&status("x86_64-pc-windows-msvc", vec![tc("abc")], vec![vec!["x86_64-pc-windows-msvc".into()]], false), &needs));
        // Same compiler build on another host is fine for ordinary jobs, not for proc macros.
        let other = status("x86_64-unknown-linux-gnu", vec![tc("abc")], vec![vec!["x86_64-pc-windows-msvc".into()]], true);
        assert!(eligible(&other, &needs));
        let pm = JobNeeds { needs_host: true, ..needs };
        assert!(!eligible(&other, &pm));
        assert!(eligible(&ok, &pm));
    }
}
