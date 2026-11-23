//! `crabfarm status` and `crabfarm status --watch` (DESIGN section 13).

use crate::cargo_cmd::query_status;
use crate::config;
use crate::ipc;
use crate::node::Reach;
use crate::proto::{Message, NodeId, PeerInfo, Status};
use crate::transport::FramedConn;
use std::future::Future;
use std::io::{ErrorKind, IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

/// Default refresh interval for `crabfarm status --watch`.
pub const DEFAULT_WATCH_INTERVAL: Duration = Duration::from_secs(1);
/// Minimum allowed `--interval` (100 ms) so polling never spins the node.
pub const MIN_WATCH_INTERVAL_MS: u64 = 100;
/// Maximum allowed `--interval` (1 hour).
pub const MAX_WATCH_INTERVAL_SECS: u64 = 3600;
/// Timeout for a single status reply in watch mode.
const WATCH_QUERY_TIMEOUT: Duration = Duration::from_secs(3);
/// Width of the ASCII slot utilization bar in watch mode.
const SLOT_BAR_WIDTH: usize = 16;

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

/// How job connections reach `p` (CF-34), from the node's `reach.<id>` counter.
fn reach_of(counters: &[(String, u64)], p: &PeerInfo) -> Reach {
    Reach::from_code(counter(counters, &format!("reach.{}", p.node_id.to_hex())))
}

fn reach_text(r: Reach) -> &'static str {
    match r {
        Reach::Untried => "-",
        Reach::Direct => "direct",
        Reach::DialBack => "dial-back",
        Reach::Failing => "FAILING",
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
            "{:<w$}  {:<28}  {:<7}  {:<9}  {}\n",
            "PEER",
            "HOST",
            "SLOTS",
            "JOBS",
            "LAST SEEN",
            w = w
        ));
        let mut failing = Vec::new();
        for (p, s) in peers {
            let reach = reach_of(counters, p);
            if s.is_some() && reach == Reach::Failing {
                failing.push(p.node_name.as_str());
            }
            match s {
                Some(s) => out.push_str(&format!(
                    "{:<w$}  {:<28}  {:<7}  {:<9}  {}\n",
                    p.node_name,
                    s.host_triple,
                    format!("{}/{}", s.slots_busy, s.slots_total),
                    reach_text(reach),
                    ago(now, p.last_seen),
                    w = w
                )),
                None => out.push_str(&format!(
                    "{:<w$}  {:<28}  {:<7}  {:<9}  {}\n",
                    p.node_name,
                    "-",
                    "offline",
                    "-",
                    ago(now, p.last_seen),
                    w = w
                )),
            }
        }
        out.push('\n');
        if !failing.is_empty() {
            out.push_str(&format!(
                "warning: jobs cannot reach {} in either direction (status gets through, job connections do not); \
                 check that the machines can open TCP connections to each other's node port (default 47070)\n\n",
                failing.join(", "),
            ));
        }
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
    let (unrecorded, changed) = (
        counter(counters, "unknown_dep.not_recorded"),
        counter(counters, "unknown_dep.changed"),
    );
    if unrecorded + changed > 0 {
        out.push_str(&format!(
            "  UnknownDependency: {unrecorded} had a dependency cargo built without crabfarm, {changed} one that changed since; \
             run `crabfarm doctor` in the project\n"
        ));
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
        "compile cache: {} hits ({} local, {} farm), {} misses, {} of rustc output reused, {} stored, {} served to peers, {} unusable, {} local results not cached: they may depend on more than the key covers
",
        local + farm,
        local,
        farm,
        counter(counters, "cache_miss"),
        mib(counter(counters, "cache_bytes_saved")),
        counter(counters, "cache_stored"),
        counter(counters, "cache_served"),
        counter(counters, "cache_corrupt") + counter(counters, "cache_error"),
        counter(counters, "cache_local_skipped"),
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

// ------------------------------------------------------------------- --watch

/// Parse `--interval <SECS>` for `crabfarm status --watch`.
pub fn parse_watch_interval(s: &str) -> Result<Duration, String> {
    let secs: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("invalid interval `{s}`: expected seconds (e.g. 1 or 0.5)"))?;
    if !secs.is_finite() || secs <= 0.0 {
        return Err(format!(
            "invalid interval `{s}`: must be a finite number greater than 0"
        ));
    }
    let min_secs = MIN_WATCH_INTERVAL_MS as f64 / 1000.0;
    if secs < min_secs {
        return Err(format!(
            "interval `{s}` is too small (minimum is {min_secs}s)"
        ));
    }
    if secs > MAX_WATCH_INTERVAL_SECS as f64 {
        return Err(format!(
            "interval `{s}` is too large (maximum is {MAX_WATCH_INTERVAL_SECS}s)"
        ));
    }
    let ms = (secs * 1000.0).round() as u64;
    Ok(Duration::from_millis(ms.max(MIN_WATCH_INTERVAL_MS)))
}

/// Format an interval compactly (`1s`, `0.5s`, `250ms`).
pub fn format_interval(d: Duration) -> String {
    let ms = d.as_millis();
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else if ms >= 1000 && ms.is_multiple_of(100) {
        format!("{}.{}s", ms / 1000, (ms % 1000) / 100)
    } else if ms < 1000 && ms.is_multiple_of(100) {
        format!("0.{}s", ms / 100)
    } else {
        format!("{ms}ms")
    }
}

/// Format elapsed watch duration (`00:05`, `01:23`, `1:02:03`).
pub fn format_elapsed(d: Duration) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Format a byte throughput rate cleanly (`512 B/s`, `48.2 KiB/s`, `3.4 MiB/s`).
pub fn format_rate(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return "0 B/s".to_string();
    }
    if bytes_per_sec < 1024.0 {
        format!("{} B/s", bytes_per_sec.round() as u64)
    } else if bytes_per_sec < 1024.0 * 1024.0 {
        format!("{:.1} KiB/s", bytes_per_sec / 1024.0)
    } else {
        format!("{:.1} MiB/s", bytes_per_sec / (1024.0 * 1024.0))
    }
}

/// ASCII utilization bar, e.g. `[####............]`.
/// Any `busy > 0` with `total > 0` shows at least one `#`, and `busy < total`
/// never fills the last cell so "almost full" is distinct from "full".
pub fn slot_bar(busy: u32, total: u32, width: usize) -> String {
    let filled = if total == 0 || busy == 0 || width == 0 {
        0
    } else if busy >= total {
        width
    } else {
        let raw = ((busy as u64 * width as u64) + (total as u64 / 2)) / total as u64;
        (raw as usize).clamp(1, width.saturating_sub(1).max(1))
    };
    let mut out = String::with_capacity(width + 2);
    out.push('[');
    for _ in 0..filled {
        out.push('#');
    }
    for _ in filled..width {
        out.push('.');
    }
    out.push(']');
    out
}

/// One status reply from the local node.
#[derive(Clone, Debug)]
pub struct StatusReply {
    pub local: Status,
    pub peers: Vec<(PeerInfo, Option<Status>)>,
    pub counters: Vec<(String, u64)>,
}

/// Monotonic counters captured at one tick for computing session deltas and rates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WatchSnapshot {
    node_id: NodeId,
    remote_ok: u64,
    remote_failed: u64,
    local_total: u64,
    early_notices: u64,
    cache_hit_local: u64,
    cache_hit_farm: u64,
    cache_miss: u64,
    cache_bytes_saved: u64,
    cache_stored: u64,
    cache_served: u64,
    bytes_up: u64,
    bytes_down: u64,
    served: u64,
}

impl WatchSnapshot {
    fn from_reply(local: &Status, counters: &[(String, u64)]) -> Self {
        let local_total: u64 = counters
            .iter()
            .filter(|(k, _)| k.starts_with("local."))
            .map(|(_, v)| *v)
            .sum();
        Self {
            node_id: local.node_id,
            remote_ok: counter(counters, "remote_ok"),
            remote_failed: counter(counters, "remote_failed")
                + counter(counters, "remote_exit_nonzero"),
            local_total,
            early_notices: counter(counters, "early_notices"),
            cache_hit_local: counter(counters, "cache_hit_local"),
            cache_hit_farm: counter(counters, "cache_hit_farm"),
            cache_miss: counter(counters, "cache_miss"),
            cache_bytes_saved: counter(counters, "cache_bytes_saved"),
            cache_stored: counter(counters, "cache_stored"),
            cache_served: counter(counters, "cache_served"),
            bytes_up: counter(counters, "bytes_up"),
            bytes_down: counter(counters, "bytes_down"),
            served: counter(counters, "served"),
        }
    }

    fn zeros(node_id: NodeId) -> Self {
        Self {
            node_id,
            remote_ok: 0,
            remote_failed: 0,
            local_total: 0,
            early_notices: 0,
            cache_hit_local: 0,
            cache_hit_farm: 0,
            cache_miss: 0,
            cache_bytes_saved: 0,
            cache_stored: 0,
            cache_served: 0,
            bytes_up: 0,
            bytes_down: 0,
            served: 0,
        }
    }

    /// True if any monotonic counter dropped or the node id changed (node restarted).
    fn reset_since(&self, earlier: &Self) -> bool {
        self.node_id != earlier.node_id
            || self.remote_ok < earlier.remote_ok
            || self.remote_failed < earlier.remote_failed
            || self.local_total < earlier.local_total
            || self.early_notices < earlier.early_notices
            || self.cache_hit_local < earlier.cache_hit_local
            || self.cache_hit_farm < earlier.cache_hit_farm
            || self.cache_miss < earlier.cache_miss
            || self.cache_bytes_saved < earlier.cache_bytes_saved
            || self.cache_stored < earlier.cache_stored
            || self.cache_served < earlier.cache_served
            || self.bytes_up < earlier.bytes_up
            || self.bytes_down < earlier.bytes_down
            || self.served < earlier.served
    }
}

/// Session deltas (since `--watch` started or node restarted) and live traffic rates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WatchDelta {
    pub remote_ok: u64,
    pub remote_failed: u64,
    pub local_total: u64,
    pub early_notices: u64,
    pub cache_hits: u64,
    pub cache_miss: u64,
    pub cache_bytes_saved: u64,
    pub served: u64,
    pub up_bps: f64,
    pub down_bps: f64,
    /// True when a node restart was detected since the watch started.
    pub node_restarted: bool,
}

/// Tracks watch session baseline and previous tick for deltas and rates.
#[derive(Default, Debug)]
pub struct WatchState {
    baseline: Option<WatchSnapshot>,
    prev: Option<(Instant, WatchSnapshot)>,
    restarted: bool,
}

impl WatchState {
    pub fn record(
        &mut self,
        local: &Status,
        counters: &[(String, u64)],
        at: Instant,
    ) -> WatchDelta {
        let cur = WatchSnapshot::from_reply(local, counters);
        let base = match self.baseline {
            None => {
                self.baseline = Some(cur);
                cur
            }
            Some(b) if cur.reset_since(&b) => {
                // The node restarted: count all activity on the new node run.
                let zero = WatchSnapshot::zeros(cur.node_id);
                self.baseline = Some(zero);
                self.prev = None;
                self.restarted = true;
                zero
            }
            Some(b) => b,
        };

        let (up_bps, down_bps) = match self.prev {
            Some((prev_at, prev)) if !cur.reset_since(&prev) => {
                let dt = at.saturating_duration_since(prev_at).as_secs_f64();
                if dt >= 0.05 {
                    (
                        cur.bytes_up.saturating_sub(prev.bytes_up) as f64 / dt,
                        cur.bytes_down.saturating_sub(prev.bytes_down) as f64 / dt,
                    )
                } else {
                    (0.0, 0.0)
                }
            }
            _ => (0.0, 0.0),
        };
        self.prev = Some((at, cur));

        WatchDelta {
            remote_ok: cur.remote_ok.saturating_sub(base.remote_ok),
            remote_failed: cur.remote_failed.saturating_sub(base.remote_failed),
            local_total: cur.local_total.saturating_sub(base.local_total),
            early_notices: cur.early_notices.saturating_sub(base.early_notices),
            cache_hits: (cur.cache_hit_local + cur.cache_hit_farm)
                .saturating_sub(base.cache_hit_local + base.cache_hit_farm),
            cache_miss: cur.cache_miss.saturating_sub(base.cache_miss),
            cache_bytes_saved: cur.cache_bytes_saved.saturating_sub(base.cache_bytes_saved),
            served: cur.served.saturating_sub(base.served),
            up_bps,
            down_bps,
            node_restarted: self.restarted,
        }
    }

    /// Clear previous-tick state when the node connection is lost so reconnect
    /// does not compute a bogus rate over the offline gap.
    pub fn note_disconnected(&mut self) {
        self.prev = None;
    }
}

/// Optional ANSI SGR styling; returns plain text when `color == false`.
#[derive(Clone, Copy)]
struct Theme {
    color: bool,
}

impl Theme {
    fn style(self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
    fn bold(self, text: &str) -> String {
        self.style("1", text)
    }
    fn dim(self, text: &str) -> String {
        self.style("2", text)
    }
    fn green(self, text: &str) -> String {
        self.style("32", text)
    }
    fn yellow(self, text: &str) -> String {
        self.style("33", text)
    }
    fn red_bold(self, text: &str) -> String {
        self.style("1;31", text)
    }
    fn cyan(self, text: &str) -> String {
        self.style("36", text)
    }
}

fn delta_suffix(delta: u64, theme: Theme) -> String {
    if delta == 0 {
        String::new()
    } else {
        format!(" {}", theme.green(&format!("(+{delta})")))
    }
}

fn format_bar_cell(busy: u32, total: u32, accepting: bool, theme: Theme) -> String {
    if !accepting && busy == 0 {
        let plain = format!("{:<23}", "not accepting");
        return theme.dim(&plain);
    }
    let bar = slot_bar(busy, total, SLOT_BAR_WIDTH);
    let ratio = format!("{busy:>2}/{total:<2}");
    if !theme.color {
        return format!("{bar} {ratio}");
    }
    let bar_colored = if busy == 0 {
        theme.dim(&bar)
    } else if total > 0 && busy >= total {
        theme.yellow(&bar)
    } else {
        theme.green(&bar)
    };
    let ratio_colored = if busy > 0 {
        theme.bold(&ratio)
    } else {
        theme.dim(&ratio)
    };
    format!("{bar_colored} {ratio_colored}")
}

/// Format one live watch frame when the node answered.
#[allow(clippy::too_many_arguments)]
pub fn format_watch_frame(
    local: &Status,
    peers: &[(PeerInfo, Option<Status>)],
    counters: &[(String, u64)],
    now: u64,
    interval: Duration,
    elapsed: Duration,
    delta: &WatchDelta,
    unresponsive: Option<&str>,
    color: bool,
    final_frame: bool,
) -> String {
    let theme = Theme { color };
    let mut out = String::new();

    // Header bar
    let header = if final_frame {
        format!(
            "crabfarm status  |  watch stopped after {}",
            format_elapsed(elapsed)
        )
    } else {
        format!(
            "crabfarm status  |  watch {}  |  elapsed {}  |  Ctrl+C to stop",
            format_interval(interval),
            format_elapsed(elapsed),
        )
    };
    out.push_str(&theme.bold(&header));
    out.push('\n');

    if let Some(note) = unresponsive {
        out.push_str(&theme.red_bold(note.trim_end()));
        out.push('\n');
    }

    // 1-second summary line
    let online_count = peers.iter().filter(|(_, s)| s.is_some()).count();
    let remote_total: u32 = peers
        .iter()
        .filter_map(|(_, s)| s.as_ref())
        .filter(|s| s.accepting)
        .map(|s| s.slots_total)
        .sum();
    let remote_busy: u32 = peers
        .iter()
        .filter_map(|(_, s)| s.as_ref())
        .filter(|s| s.accepting)
        .map(|s| s.slots_busy.min(s.slots_total))
        .sum();
    let remote_free: u32 = peers
        .iter()
        .filter_map(|(_, s)| s.as_ref())
        .filter(|s| s.accepting)
        .map(|s| s.slots_total.saturating_sub(s.slots_busy))
        .sum();
    let running_local = counter(counters, "running_local");
    let running_remote = counter(counters, "running_remote");
    let local_slots = counter(counters, "local_slots") as u32;

    let peers_part = format!("{online_count}/{} peers online", peers.len());
    let peers_styled = if peers.is_empty() || online_count == 0 {
        theme.yellow(&peers_part)
    } else {
        theme.green(&peers_part)
    };
    let active_part = format!("{running_remote} remote, {running_local} local");
    let active_styled = if running_remote > 0 || running_local > 0 {
        theme.cyan(&active_part)
    } else {
        theme.dim(&active_part)
    };
    out.push_str(&format!(
        "farm: {peers_styled}  |  remote slots: {remote_busy}/{remote_total} busy ({remote_free} free)  |  active: {active_styled}\n\n",
    ));

    // Unified nodes & peers table with visual slot bars
    let local_build_label = format!("{} (local)", local.node_name);
    let local_serve_label = format!("{} (serve)", local.node_name);
    let name_w = peers
        .iter()
        .map(|(p, _)| p.node_name.chars().count())
        .chain([
            4,
            local_build_label.chars().count(),
            local_serve_label.chars().count(),
        ])
        .max()
        .unwrap_or(4);
    let host_w = peers
        .iter()
        .filter_map(|(_, s)| s.as_ref().map(|st| st.host_triple.chars().count()))
        .chain([4, local.host_triple.chars().count()])
        .max()
        .unwrap_or(4);

    let table_hdr = format!(
        "{:<name_w$}  {:<host_w$}  {:<23}  {:<9}  {}",
        "NODE", "HOST", "SLOTS", "MODE", "LAST SEEN",
    );
    out.push_str(&theme.dim(&table_hdr));
    out.push('\n');

    // Local build slots row
    out.push_str(&format!(
        "{:<name_w$}  {:<host_w$}  {}  {:<9}  {}\n",
        local_build_label,
        local.host_triple,
        format_bar_cell(running_local as u32, local_slots, true, theme),
        "local",
        "now",
    ));
    // Local serving slots row
    out.push_str(&format!(
        "{:<name_w$}  {:<host_w$}  {}  {:<9}  {}\n",
        local_serve_label,
        local.host_triple,
        format_bar_cell(local.slots_busy, local.slots_total, local.accepting, theme),
        "serving",
        "now",
    ));

    let mut failing = Vec::new();
    for (p, s) in peers {
        let reach = reach_of(counters, p);
        if s.is_some() && reach == Reach::Failing {
            failing.push(p.node_name.as_str());
        }
        match s {
            Some(s) => {
                let reach_raw = format!("{:<9}", reach_text(reach));
                let reach_styled = match reach {
                    Reach::Direct | Reach::DialBack => theme.green(&reach_raw),
                    Reach::Failing => theme.red_bold(&reach_raw),
                    Reach::Untried => theme.dim(&reach_raw),
                };
                out.push_str(&format!(
                    "{:<name_w$}  {:<host_w$}  {}  {}  {}\n",
                    p.node_name,
                    s.host_triple,
                    format_bar_cell(s.slots_busy, s.slots_total, s.accepting, theme),
                    reach_styled,
                    ago(now, p.last_seen),
                ));
            }
            None => {
                let offline_line = format!(
                    "{:<name_w$}  {:<host_w$}  {:<23}  {:<9}  {}",
                    p.node_name,
                    "-",
                    "offline",
                    "-",
                    ago(now, p.last_seen),
                );
                out.push_str(&theme.dim(&offline_line));
                out.push('\n');
            }
        }
    }
    out.push('\n');

    if !failing.is_empty() {
        let warn = format!(
            "warning: jobs cannot reach {} in either direction (status gets through, job connections do not); \
             check that the machines can open TCP connections to each other's node port (default 47070)",
            failing.join(", "),
        );
        out.push_str(&theme.red_bold(&warn));
        out.push_str("\n\n");
    }

    // Counters with session deltas & live traffic rates
    let local_total: u64 = counters
        .iter()
        .filter(|(k, _)| k.starts_with("local."))
        .map(|(_, v)| *v)
        .sum();
    let remote_ok = counter(counters, "remote_ok");
    let remote_failed =
        counter(counters, "remote_failed") + counter(counters, "remote_exit_nonzero");
    let failed_delta = if delta.remote_failed > 0 {
        format!(
            " {}",
            theme.red_bold(&format!("(+{})", delta.remote_failed))
        )
    } else {
        String::new()
    };
    out.push_str(&format!(
        "jobs: {} remote ok{}, {} remote failed{}, {} local{}, {} running remote\n",
        remote_ok,
        delta_suffix(delta.remote_ok, theme),
        remote_failed,
        failed_delta,
        local_total,
        delta_suffix(delta.local_total, theme),
        running_remote,
    ));

    let reasons: Vec<String> = counters
        .iter()
        .filter_map(|(k, v)| k.strip_prefix("local.").map(|r| format!("{r} {v}")))
        .collect();
    if !reasons.is_empty() {
        out.push_str(&format!("local because: {}\n", reasons.join(", ")));
    }
    let (unrecorded, changed) = (
        counter(counters, "unknown_dep.not_recorded"),
        counter(counters, "unknown_dep.changed"),
    );
    if unrecorded + changed > 0 {
        out.push_str(&format!(
            "  UnknownDependency: {unrecorded} had a dependency cargo built without crabfarm, {changed} one that changed since; \
             run `crabfarm doctor` in the project\n"
        ));
    }
    let early = counter(counters, "early_notices");
    out.push_str(&format!(
        "pipelining: {} artifact notices relayed before their job ended{}\n",
        early,
        delta_suffix(delta.early_notices, theme),
    ));

    if counter(counters, "cache_enabled") == 0 {
        out.push_str("compile cache: off\n");
    } else {
        let cl = counter(counters, "cache_hit_local");
        let cf = counter(counters, "cache_hit_farm");
        let saved_delta = if delta.cache_bytes_saved > 0 {
            format!(
                " {}",
                theme.green(&format!("(+{})", mib(delta.cache_bytes_saved)))
            )
        } else {
            String::new()
        };
        out.push_str(&format!(
            "compile cache: {} hits{} ({} local, {} farm), {} misses{}, {} reused{}, {} stored, {} served, {} unusable, {} local not cached\n",
            cl + cf,
            delta_suffix(delta.cache_hits, theme),
            cl,
            cf,
            counter(counters, "cache_miss"),
            delta_suffix(delta.cache_miss, theme),
            mib(counter(counters, "cache_bytes_saved")),
            saved_delta,
            counter(counters, "cache_stored"),
            counter(counters, "cache_served"),
            counter(counters, "cache_corrupt") + counter(counters, "cache_error"),
            counter(counters, "cache_local_skipped"),
        ));
    }

    let up_rate = if delta.up_bps >= 1.0 {
        format!(" ({})", theme.cyan(&format_rate(delta.up_bps)))
    } else {
        String::new()
    };
    let down_rate = if delta.down_bps >= 1.0 {
        format!(" ({})", theme.cyan(&format_rate(delta.down_bps)))
    } else {
        String::new()
    };
    out.push_str(&format!(
        "traffic: {} up{}, {} down{}, {} jobs served for peers{}\n",
        mib(counter(counters, "bytes_up")),
        up_rate,
        mib(counter(counters, "bytes_down")),
        down_rate,
        counter(counters, "served"),
        delta_suffix(delta.served, theme),
    ));

    if delta.node_restarted {
        out.push_str(&theme.dim("(node restarted during watch; session deltas reset)\n"));
    }

    out
}

/// Format one watch frame when the local node is offline / not answering.
pub fn format_watch_offline(
    interval: Duration,
    elapsed: Duration,
    unresponsive: Option<&str>,
    color: bool,
    final_frame: bool,
) -> String {
    let theme = Theme { color };
    let mut out = String::new();
    let header = if final_frame {
        format!(
            "crabfarm status  |  watch stopped after {}",
            format_elapsed(elapsed)
        )
    } else {
        format!(
            "crabfarm status  |  watch {}  |  elapsed {}  |  Ctrl+C to stop",
            format_interval(interval),
            format_elapsed(elapsed),
        )
    };
    out.push_str(&theme.bold(&header));
    out.push('\n');
    if let Some(note) = unresponsive {
        out.push_str(&theme.red_bold(note.trim_end()));
        out.push('\n');
    }
    let msg = "node: not running (waiting for node to start; run `crabfarm node` or `crabfarm service install`)";
    out.push_str(&theme.yellow(msg));
    out.push('\n');
    out
}

/// Wrap `body` in flicker-free cursor-home + erase-to-EOL + erase-to-EOS ANSI sequences.
pub fn render_ansi_frame(body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 128);
    out.push_str("\x1b[H");
    for line in body.lines() {
        out.push_str(line);
        out.push_str("\x1b[K\n");
    }
    out.push_str("\x1b[J");
    out
}

/// RAII terminal guard for alternate-screen ANSI rendering.
/// Restores cursor visibility, normal screen buffer, and Windows console mode on Drop.
struct TerminalGuard {
    ansi: bool,
    color: bool,
    #[cfg(windows)]
    saved_mode: Option<(windows_sys::Win32::Foundation::HANDLE, u32)>,
}

impl TerminalGuard {
    fn enter() -> Self {
        let is_tty = std::io::stdout().is_terminal();
        if !is_tty {
            return Self {
                ansi: false,
                color: false,
                #[cfg(windows)]
                saved_mode: None,
            };
        }
        #[cfg(not(windows))]
        if std::env::var_os("TERM").is_some_and(|v| v == "dumb") {
            return Self {
                ansi: false,
                color: false,
            };
        }
        #[cfg(windows)]
        let (ansi, saved_mode) = enable_windows_vt();
        #[cfg(not(windows))]
        let ansi = true;

        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let color = ansi && !no_color;
        if ansi {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x1b[?1049h\x1b[?25l");
            let _ = out.flush();
        }
        Self {
            ansi,
            color,
            #[cfg(windows)]
            saved_mode,
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.ansi {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x1b[?25h\x1b[?1049l");
            let _ = out.flush();
        }
        #[cfg(windows)]
        if let Some((handle, mode)) = self.saved_mode {
            // SAFETY: `handle` is the valid stdout console handle returned by `GetStdHandle`
            // and `mode` is the exact console mode saved before enabling VT processing.
            unsafe {
                windows_sys::Win32::System::Console::SetConsoleMode(handle, mode);
            }
        }
    }
}

#[cfg(windows)]
fn enable_windows_vt() -> (bool, Option<(windows_sys::Win32::Foundation::HANDLE, u32)>) {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE,
        SetConsoleMode,
    };
    // SAFETY: `GetStdHandle` and `GetConsoleMode`/`SetConsoleMode` are standard Win32 console
    // calls on the process stdout handle; we validate the handle before touching console mode.
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return (false, None);
        }
        let mut mode: u32 = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return (false, None);
        }
        if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0 {
            return (true, None);
        }
        if SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0 {
            return (false, None);
        }
        (true, Some((handle, mode)))
    }
}

/// Persistent IPC client for `--watch`: keeps one authenticated connection open
/// across ticks and reconnects automatically if the node restarts or goes away.
pub struct IpcStatusClient {
    conn: Option<FramedConn<TcpStream>>,
}

impl IpcStatusClient {
    pub fn new() -> Self {
        Self { conn: None }
    }

    pub async fn query(&mut self, data_dir: &Path) -> Option<StatusReply> {
        for attempt in 0..2 {
            let reused = self.conn.is_some();
            if !reused {
                match ipc::connect(data_dir).await {
                    Ok(c) => self.conn = Some(c),
                    Err(_) => return None,
                }
            }
            let conn = self.conn.as_mut()?;
            if conn.send(&Message::QueryStatus).await.is_ok()
                && let Ok(Ok(Some(Message::FarmStatus {
                    local,
                    peers,
                    counters,
                }))) = tokio::time::timeout(WATCH_QUERY_TIMEOUT, conn.recv()).await
            {
                return Some(StatusReply {
                    local,
                    peers,
                    counters,
                });
            }
            // Connection dropped or timed out; only retry immediately when we
            // tried to reuse an older connection (e.g. node restarted).
            self.conn = None;
            if !reused || attempt == 1 {
                return None;
            }
        }
        None
    }
}

impl Default for IpcStatusClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of one poll in [`watch_loop`].
pub struct WatchSample {
    pub reply: Option<StatusReply>,
    pub unresponsive: Option<String>,
    pub now_unix: u64,
}

/// Source of live status samples for [`watch_loop`].
pub trait WatchSource {
    fn poll(&mut self) -> impl Future<Output = WatchSample>;
}

/// Real IPC-backed [`WatchSource`] that keeps one authenticated connection open.
pub struct IpcWatchSource {
    data_dir: std::path::PathBuf,
    client: IpcStatusClient,
}

impl IpcWatchSource {
    pub fn new(data_dir: std::path::PathBuf) -> Self {
        Self {
            data_dir,
            client: IpcStatusClient::new(),
        }
    }
}

impl WatchSource for IpcWatchSource {
    async fn poll(&mut self) -> WatchSample {
        let now_unix = crate::node::unix_now();
        let unresponsive = unresponsive_note(ipc::unresponsive_until(&self.data_dir), now_unix);
        let reply = self.client.query(&self.data_dir).await;
        WatchSample {
            reply,
            unresponsive,
            now_unix,
        }
    }
}

/// Core testable watch loop. Returns the final formatted frame (for printing
/// after leaving the alternate screen on TTY) when stopped cleanly.
pub async fn watch_loop<W, S, Stop>(
    out: &mut W,
    interval: Duration,
    ansi: bool,
    color: bool,
    source: &mut S,
    stop: Stop,
) -> std::io::Result<Option<String>>
where
    W: Write,
    S: WatchSource,
    Stop: Future<Output = ()>,
{
    let started = Instant::now();
    let mut state = WatchState::default();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stop = std::pin::pin!(stop);
    let mut tick_count: u64 = 0;
    let mut last_final: Option<String> = None;

    loop {
        tokio::select! {
            biased;
            _ = &mut stop => break,
            _ = ticker.tick() => {}
        }

        let sample = tokio::select! {
            biased;
            _ = &mut stop => break,
            s = source.poll() => s,
        };

        let now_inst = Instant::now();
        let elapsed = now_inst.saturating_duration_since(started);
        let (frame, final_frame) = match sample.reply {
            Some(ref r) => {
                let delta = state.record(&r.local, &r.counters, now_inst);
                let live = format_watch_frame(
                    &r.local,
                    &r.peers,
                    &r.counters,
                    sample.now_unix,
                    interval,
                    elapsed,
                    &delta,
                    sample.unresponsive.as_deref(),
                    color,
                    false,
                );
                let done = format_watch_frame(
                    &r.local,
                    &r.peers,
                    &r.counters,
                    sample.now_unix,
                    interval,
                    elapsed,
                    &delta,
                    sample.unresponsive.as_deref(),
                    color,
                    true,
                );
                (live, done)
            }
            None => {
                state.note_disconnected();
                let live = format_watch_offline(
                    interval,
                    elapsed,
                    sample.unresponsive.as_deref(),
                    color,
                    false,
                );
                let done = format_watch_offline(
                    interval,
                    elapsed,
                    sample.unresponsive.as_deref(),
                    color,
                    true,
                );
                (live, done)
            }
        };
        last_final = Some(final_frame);

        let write_res = if ansi {
            out.write_all(render_ansi_frame(&frame).as_bytes())
                .and_then(|()| out.flush())
        } else {
            let prefix = if tick_count > 0 { "\n" } else { "" };
            out.write_all(prefix.as_bytes())
                .and_then(|()| out.write_all(frame.as_bytes()))
                .and_then(|()| out.flush())
        };
        match write_res {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::BrokenPipe => return Ok(None),
            Err(e) => return Err(e),
        }
        tick_count += 1;
    }

    Ok(last_final)
}

/// Run `crabfarm status --watch` until Ctrl+C.
pub fn run_watch(interval: Duration) -> i32 {
    let Ok(data_dir) = config::data_dir() else {
        eprintln!("crabfarm: cannot find the data directory");
        return 1;
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        eprintln!("crabfarm: failed to start async runtime");
        return 1;
    };

    let guard = TerminalGuard::enter();
    let ansi = guard.ansi;
    let color = guard.color;

    let result = rt.block_on(async {
        let mut source = IpcWatchSource::new(data_dir);
        let mut stdout = std::io::stdout();
        let stop = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        watch_loop(&mut stdout, interval, ansi, color, &mut source, stop).await
    });

    // Leave alternate screen before printing the final snapshot to scrollback.
    drop(guard);

    match result {
        Ok(Some(final_frame)) if ansi => {
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(final_frame.as_bytes());
            let _ = stdout.flush();
            0
        }
        Ok(_) => 0,
        Err(e) => {
            eprintln!("crabfarm: {e}");
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
            c("cache_local_skipped", 6),
        ]);
        assert!(
            line.contains("7 hits (5 local, 2 farm), 3 misses, 4.0 MiB"),
            "{line}"
        );
        assert!(
            line.trim_end().ends_with(
                "6 local results not cached: they may depend on more than the key covers"
            ),
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
            (format!("reach.{}", NodeId([2; 16]).to_hex()), 2),
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
        assert!(t.contains("dial-back"), "{t}");
        assert!(!t.contains("warning: jobs cannot reach"));
    }

    #[test]
    fn unknown_dependencies_are_split_by_cause() {
        let c = |k: &str, v: u64| (k.to_string(), v);
        let counters = vec![
            c("local.UnknownDependency", 50),
            c("unknown_dep.not_recorded", 49),
            c("unknown_dep.changed", 1),
        ];
        let t = format_status(&st("me", 8, 0), &[], &counters, 1000);
        assert!(
            t.contains(
                "49 had a dependency cargo built without crabfarm, 1 one that changed since"
            ),
            "{t}"
        );
        let t = format_status(&st("me", 8, 0), &[], &[c("local.NoPeer", 2)], 1000);
        assert!(!t.contains("UnknownDependency:"), "{t}");
    }

    #[test]
    fn a_peer_jobs_cannot_reach_is_named() {
        let p = PeerInfo {
            node_id: NodeId([3; 16]),
            node_name: "desktop".into(),
            addrs: vec![],
            last_seen: 999,
        };
        let counters = vec![(format!("reach.{}", NodeId([3; 16]).to_hex()), 3)];
        let t = format_status(
            &st("me", 8, 0),
            &[(p, Some(st("desktop", 16, 0)))],
            &counters,
            1000,
        );
        assert!(t.contains("FAILING"), "{t}");
        assert!(t.contains("warning: jobs cannot reach desktop"), "{t}");
    }

    #[test]
    fn watch_interval_parser_accepts_valid_and_rejects_bad_values() {
        assert_eq!(parse_watch_interval("1"), Ok(Duration::from_secs(1)));
        assert_eq!(parse_watch_interval("0.5"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_watch_interval("0.1"), Ok(Duration::from_millis(100)));
        assert_eq!(
            parse_watch_interval("2.25"),
            Ok(Duration::from_millis(2250))
        );
        assert_eq!(parse_watch_interval("3600"), Ok(Duration::from_secs(3600)));
        for bad in ["0", "-1", "0.05", "3601", "NaN", "inf", "-inf", "abc", ""] {
            assert!(parse_watch_interval(bad).is_err(), "should reject `{bad}`");
        }
    }

    #[test]
    fn slot_bar_distinguishes_empty_partial_and_full() {
        assert_eq!(slot_bar(0, 16, 16), "[................]");
        assert_eq!(slot_bar(16, 16, 16), "[################]");
        assert_eq!(slot_bar(20, 16, 16), "[################]");
        assert_eq!(slot_bar(0, 0, 16), "[................]");
        // Even 1/64 visibly lights up one cell, and 63/64 leaves one cell open.
        assert_eq!(slot_bar(1, 64, 16), "[#...............]");
        assert_eq!(slot_bar(63, 64, 16), "[###############.]");
        assert_eq!(slot_bar(8, 16, 16), "[########........]");
    }

    #[test]
    fn format_helpers_render_clean_units() {
        assert_eq!(format_interval(Duration::from_secs(1)), "1s");
        assert_eq!(format_interval(Duration::from_millis(500)), "0.5s");
        assert_eq!(format_interval(Duration::from_millis(250)), "250ms");
        assert_eq!(format_interval(Duration::from_millis(1500)), "1.5s");

        assert_eq!(format_elapsed(Duration::from_secs(5)), "00:05");
        assert_eq!(format_elapsed(Duration::from_secs(125)), "02:05");
        assert_eq!(format_elapsed(Duration::from_secs(3661)), "1:01:01");

        assert_eq!(format_rate(0.0), "0 B/s");
        assert_eq!(format_rate(512.0), "512 B/s");
        assert_eq!(format_rate(2048.0), "2.0 KiB/s");
        assert_eq!(format_rate(3.5 * 1024.0 * 1024.0), "3.5 MiB/s");
    }

    #[test]
    fn watch_state_tracks_session_deltas_rates_and_node_restarts() {
        let mut ws = WatchState::default();
        let t0 = Instant::now();
        let c = |k: &str, v: u64| (k.to_string(), v);
        let me = st("me", 8, 0);

        // Initial tick establishes baseline: zero session deltas and zero rates.
        let d0 = ws.record(
            &me,
            &[
                c("remote_ok", 10),
                c("local.NoPeer", 2),
                c("bytes_up", 1_000_000),
                c("bytes_down", 2_000_000),
                c("cache_hit_local", 4),
            ],
            t0,
        );
        assert_eq!(d0.remote_ok, 0);
        assert_eq!(d0.local_total, 0);
        assert_eq!(d0.cache_hits, 0);
        assert_eq!(d0.up_bps, 0.0);
        assert!(!d0.node_restarted);

        // Second tick 2s later: session deltas accumulate and rates reflect last interval.
        let d1 = ws.record(
            &me,
            &[
                c("remote_ok", 15),
                c("local.NoPeer", 3),
                c("bytes_up", 3_000_000),
                c("bytes_down", 6_000_000),
                c("cache_hit_local", 6),
                c("cache_hit_farm", 1),
            ],
            t0 + Duration::from_secs(2),
        );
        assert_eq!(d1.remote_ok, 5);
        assert_eq!(d1.local_total, 1);
        assert_eq!(d1.cache_hits, 3);
        assert!((d1.up_bps - 1_000_000.0).abs() < 1.0, "{}", d1.up_bps);
        assert!((d1.down_bps - 2_000_000.0).abs() < 1.0, "{}", d1.down_bps);
        assert!(!d1.node_restarted);

        // Node restart (counters drop): baseline resets to zero, flag turns true, no bogus rate.
        let d2 = ws.record(
            &me,
            &[c("remote_ok", 2), c("bytes_up", 500_000)],
            t0 + Duration::from_secs(4),
        );
        assert_eq!(d2.remote_ok, 2);
        assert_eq!(d2.up_bps, 0.0);
        assert!(d2.node_restarted);
    }

    #[test]
    fn watch_frame_renders_bars_deltas_rates_and_warnings() {
        let peer_id = NodeId([2; 16]);
        let mut paused = st("paused", 8, 0);
        paused.accepting = false;
        let peers = vec![
            (
                PeerInfo {
                    node_id: peer_id,
                    node_name: "desktop".into(),
                    addrs: vec![],
                    last_seen: 998,
                },
                Some(st("desktop", 16, 12)),
            ),
            (
                PeerInfo {
                    node_id: NodeId([3; 16]),
                    node_name: "paused".into(),
                    addrs: vec![],
                    last_seen: 999,
                },
                Some(paused),
            ),
            (
                PeerInfo {
                    node_id: NodeId([4; 16]),
                    node_name: "laptop".into(),
                    addrs: vec![],
                    last_seen: 100,
                },
                None,
            ),
        ];
        let c = |k: &str, v: u64| (k.to_string(), v);
        let counters = vec![
            (format!("reach.{}", peer_id.to_hex()), 2),
            c("local_slots", 8),
            c("running_local", 2),
            c("running_remote", 6),
            c("remote_ok", 40),
            c("local.NoPeer", 3),
            c("cache_enabled", 1),
            c("cache_hit_local", 10),
            c("cache_hit_farm", 2),
            c("cache_miss", 4),
            c("bytes_up", 10 * 1024 * 1024),
            c("bytes_down", 20 * 1024 * 1024),
        ];
        let delta = WatchDelta {
            remote_ok: 7,
            local_total: 1,
            cache_hits: 3,
            up_bps: 2.0 * 1024.0 * 1024.0,
            down_bps: 512.0 * 1024.0,
            ..WatchDelta::default()
        };
        let frame = format_watch_frame(
            &st("me", 8, 0),
            &peers,
            &counters,
            1000,
            Duration::from_secs(1),
            Duration::from_secs(42),
            &delta,
            None,
            false,
            false,
        );
        assert!(frame.contains("watch 1s  |  elapsed 00:42"), "{frame}");
        assert!(frame.contains("2/3 peers online"), "{frame}");
        assert!(
            frame.contains("remote slots: 12/16 busy (4 free)"),
            "{frame}"
        );
        assert!(frame.contains("active: 6 remote, 2 local"), "{frame}");
        assert!(frame.contains("[############....] 12/16"), "{frame}");
        assert!(frame.contains("not accepting"), "{frame}");
        assert!(frame.contains("offline"), "{frame}");
        assert!(frame.contains("40 remote ok (+7)"), "{frame}");
        assert!(frame.contains("3 local (+1)"), "{frame}");
        assert!(frame.contains("12 hits (+3)"), "{frame}");
        assert!(frame.contains("2.0 MiB/s"), "{frame}");
        assert!(frame.contains("512.0 KiB/s"), "{frame}");
        assert!(!frame.contains("\x1b["), "plain mode must not emit ANSI");

        let ansi_frame = render_ansi_frame(&frame);
        assert!(ansi_frame.starts_with("\x1b[H"));
        assert!(ansi_frame.contains("\x1b[K\n"));
        assert!(ansi_frame.ends_with("\x1b[J"));
    }

    #[tokio::test]
    async fn watch_loop_renders_online_offline_and_stops_cleanly() {
        struct Scripted {
            samples: std::collections::VecDeque<WatchSample>,
            stop: Option<tokio::sync::oneshot::Sender<()>>,
        }
        impl WatchSource for Scripted {
            async fn poll(&mut self) -> WatchSample {
                let s = self.samples.pop_front().unwrap();
                if self.samples.is_empty()
                    && let Some(tx) = self.stop.take()
                {
                    let _ = tx.send(());
                }
                s
            }
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let make = |ok: u64| WatchSample {
            reply: Some(StatusReply {
                local: st("me", 8, 0),
                peers: vec![],
                counters: vec![("remote_ok".into(), ok), ("local_slots".into(), 8)],
            }),
            unresponsive: None,
            now_unix: 1000,
        };
        let mut src = Scripted {
            samples: std::collections::VecDeque::from([
                make(10),
                WatchSample {
                    reply: None,
                    unresponsive: None,
                    now_unix: 1001,
                },
                make(14),
            ]),
            stop: Some(tx),
        };
        let mut buf = Vec::new();
        let final_frame = watch_loop(
            &mut buf,
            Duration::from_millis(10),
            false,
            false,
            &mut src,
            async {
                let _ = rx.await;
            },
        )
        .await
        .unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("10 remote ok"), "{output}");
        assert!(output.contains("node: not running"), "{output}");
        assert!(output.contains("14 remote ok (+4)"), "{output}");
        assert!(
            final_frame.as_deref().is_some_and(
                |f| f.contains("watch stopped after") && f.contains("14 remote ok (+4)")
            ),
            "{final_frame:?}"
        );
    }

    #[tokio::test]
    async fn ipc_status_client_reuses_connection_and_reconnects_after_drop() {
        let d = tempfile::tempdir().unwrap();
        let (listener, token) = ipc::bind(d.path()).await.unwrap();
        let accepts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accepts_srv = accepts.clone();
        let server = tokio::spawn(async move {
            // Connection 1 serves two QueryStatus requests then closes.
            let (s1, _) = listener.accept().await.unwrap();
            accepts_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut c1 = FramedConn::new(s1);
            assert_eq!(
                ipc::authenticate(&mut c1, &token).await,
                ipc::Auth::Accepted
            );
            for n in [1u64, 2u64] {
                assert!(matches!(
                    c1.recv().await.unwrap(),
                    Some(Message::QueryStatus)
                ));
                c1.send(&Message::FarmStatus {
                    local: st("me", 8, 0),
                    peers: vec![],
                    counters: vec![("remote_ok".into(), n)],
                })
                .await
                .unwrap();
            }
            drop(c1);

            // Connection 2 serves the reconnected query.
            let (s2, _) = listener.accept().await.unwrap();
            accepts_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut c2 = FramedConn::new(s2);
            assert_eq!(
                ipc::authenticate(&mut c2, &token).await,
                ipc::Auth::Accepted
            );
            assert!(matches!(
                c2.recv().await.unwrap(),
                Some(Message::QueryStatus)
            ));
            c2.send(&Message::FarmStatus {
                local: st("me", 8, 0),
                peers: vec![],
                counters: vec![("remote_ok".into(), 9)],
            })
            .await
            .unwrap();
        });

        let mut client = IpcStatusClient::new();
        let r1 = client.query(d.path()).await.expect("first query");
        let r2 = client.query(d.path()).await.expect("reused connection");
        assert_eq!(counter(&r1.counters, "remote_ok"), 1);
        assert_eq!(counter(&r2.counters, "remote_ok"), 2);
        assert_eq!(accepts.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Next query detects closed connection and reconnects automatically.
        let r3 = client.query(d.path()).await.expect("reconnected query");
        assert_eq!(counter(&r3.counters, "remote_ok"), 9);
        assert_eq!(accepts.load(std::sync::atomic::Ordering::SeqCst), 2);
        server.await.unwrap();
    }
}
