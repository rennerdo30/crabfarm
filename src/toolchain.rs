//! Package C. Toolchain identity, lookup and installation (DESIGN section 7).

use crate::proto::ToolchainId;
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Timeout of the cheap queries (`rustc -vV`, `rustup toolchain list`, ...).
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// Timeout of one rustup install child.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Timeout of one `rustup toolchain uninstall` child.
const UNINSTALL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Timeout of the kill helper (`taskkill` / `kill`).
const KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// Default of the `max_auto_toolchains` config value.
pub const DEFAULT_MAX_AUTO_TOOLCHAINS: usize = 4;
/// Remembered failed (spec, target) pairs; the least recently used is dropped.
const FAILED_CAP: usize = 64;
/// File in the data dir listing the toolchains this node installed itself.
const AUTO_FILE: &str = "auto-toolchains.toml";

/// Parse `rustc -vV` output.
pub fn parse_version_verbose(out: &str) -> anyhow::Result<ToolchainId> {
    let (mut release, mut hash, mut date, mut host) = (None, None, None, None);
    for line in out.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim().to_string();
        match k.trim() {
            "release" => release = Some(v),
            "commit-hash" => hash = Some(v),
            "commit-date" => date = Some(v),
            "host" => host = Some(v),
            _ => {}
        }
    }
    let release = release.context("rustc -vV: no `release` line")?;
    let commit_hash = hash.context("rustc -vV: no `commit-hash` line")?;
    let host = host.context("rustc -vV: no `host` line")?;
    // Builds from source tarballs report `unknown`; such a toolchain can never
    // be proven equal to another one, so it is rejected instead of matched.
    if commit_hash.is_empty() || commit_hash == "unknown" {
        bail!("rustc -vV: toolchain has no commit hash");
    }
    Ok(ToolchainId {
        release,
        commit_hash,
        commit_date: date.unwrap_or_default(),
        host,
    })
}

/// Run `<rustc> -vV` (timeout 30 s) and parse it.
pub fn query(rustc: &Path) -> anyhow::Result<ToolchainId> {
    let mut cmd = Command::new(rustc);
    cmd.arg("-vV");
    let out =
        run_capture(cmd, QUERY_TIMEOUT).with_context(|| format!("run {} -vV", rustc.display()))?;
    parse_version_verbose(&out)
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

#[derive(Default)]
struct State {
    installed: Vec<Installed>,
    /// The single background install, with the PID of its current rustup child.
    installing: Option<InstallState>,
    /// (spec, target) pairs whose install failed in this process; not retried,
    /// so an unavailable toolchain does not trigger a download loop.
    failed: FailedSet,
    /// rustup names of the toolchains this node installed on its own (a peer
    /// asked for them); persisted in `AUTO_FILE`, counted against the cap.
    auto: Vec<String>,
}

/// Bounded LRU set: peers choose the specs, so the set must not grow with them.
#[derive(Default)]
struct FailedSet {
    order: VecDeque<(String, String)>,
}

impl FailedSet {
    /// True when `key` is present; a hit counts as a use.
    fn contains(&mut self, key: &(String, String)) -> bool {
        match self.order.iter().position(|k| k == key) {
            Some(i) => {
                if let Some(k) = self.order.remove(i) {
                    self.order.push_back(k);
                }
                true
            }
            None => false,
        }
    }

    fn insert(&mut self, key: (String, String)) {
        if let Some(i) = self.order.iter().position(|k| *k == key) {
            self.order.remove(i);
        }
        self.order.push_back(key);
        while self.order.len() > FAILED_CAP {
            self.order.pop_front();
        }
    }
}

struct InstallState {
    pid: Option<u32>,
}

struct Inner {
    data_dir: PathBuf,
    auto_install: bool,
    /// Most toolchains this node installs on its own (`max_auto_toolchains`).
    max_auto: usize,
    state: Mutex<State>,
}

/// Executing-peer toolchain registry. Thread-safe; installs run in the
/// background with a 30-minute timeout, child PIDs recorded and only those
/// killed on timeout or shutdown.
#[derive(Clone)]
pub struct Toolchains {
    inner: Arc<Inner>,
}

impl Toolchains {
    /// Scan `rustup toolchain list` (+ `rustc -vV` per toolchain, cached on
    /// disk in `<data>/toolchains.toml` by toolchain dir mtime).
    pub fn scan(data_dir: &Path, auto_install: bool) -> anyhow::Result<Toolchains> {
        Self::scan_with(data_dir, auto_install, DEFAULT_MAX_AUTO_TOOLCHAINS)
    }

    /// Like [`Toolchains::scan`] with an explicit `max_auto_toolchains`.
    pub fn scan_with(
        data_dir: &Path,
        auto_install: bool,
        max_auto: usize,
    ) -> anyhow::Result<Toolchains> {
        let installed = scan_installed(data_dir)?;
        Ok(Self::with_installed_cap(
            data_dir,
            auto_install,
            max_auto,
            installed,
        ))
    }

    /// Registry over a known list (no rustup access); used by tests.
    pub fn with_installed(
        data_dir: &Path,
        auto_install: bool,
        installed: Vec<Installed>,
    ) -> Toolchains {
        Self::with_installed_cap(
            data_dir,
            auto_install,
            DEFAULT_MAX_AUTO_TOOLCHAINS,
            installed,
        )
    }

    /// [`Toolchains::with_installed`] with an explicit cap.
    pub fn with_installed_cap(
        data_dir: &Path,
        auto_install: bool,
        max_auto: usize,
        installed: Vec<Installed>,
    ) -> Toolchains {
        let auto = load_auto(data_dir, &installed);
        Toolchains {
            inner: Arc::new(Inner {
                data_dir: data_dir.to_path_buf(),
                auto_install,
                max_auto,
                state: Mutex::new(State {
                    installed,
                    auto,
                    ..State::default()
                }),
            }),
        }
    }

    /// Re-read the machine's toolchains (after an install).
    pub fn rescan(&self) -> anyhow::Result<()> {
        let installed = scan_installed(&self.inner.data_dir)?;
        let mut st = self.lock();
        // A toolchain the user removed by hand no longer counts against the cap.
        st.auto.retain(|n| installed.iter().any(|t| t.name == *n));
        st.installed = installed;
        Ok(())
    }

    /// Toolchains this node installed on its own (tests and diagnostics).
    pub fn auto_installed(&self) -> Vec<String> {
        self.lock().auto.clone()
    }

    fn save_auto(&self, names: &[String]) {
        if let Err(e) = write_auto(&self.inner.data_dir, names) {
            tracing::warn!("could not persist {AUTO_FILE}: {e:#}");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic while holding the lock leaves plain data behind; keep going.
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Installed toolchains for `Status`.
    pub fn list(&self) -> Vec<Installed> {
        self.lock().installed.clone()
    }

    /// True while a background install is running.
    pub fn is_installing(&self) -> bool {
        self.lock().installing.is_some()
    }

    /// Kill the rustup child of a running install, if any (node shutdown).
    /// Only the recorded PID is touched.
    pub fn shutdown(&self) {
        let pid = self.lock().installing.as_ref().and_then(|i| i.pid);
        if let Some(pid) = pid {
            kill_pid(pid);
        }
    }

    /// Resolve a toolchain that can build `target` for `id` right now.
    /// When missing and auto_install is on, start installing (toolchain
    /// and/or target) in the background and return `None`.
    pub fn resolve(&self, id: &ToolchainId, target: &str) -> Option<Installed> {
        let spec = Self::install_spec(id);
        let mut st = self.lock();
        let found = st
            .installed
            .iter()
            .find(|t| t.id.same_compiler(id))
            .cloned();
        if let Some(t) = &found
            && t.targets.iter().any(|x| x == target)
        {
            return found;
        }
        if !self.inner.auto_install || st.installing.is_some() {
            return None;
        }
        // The id comes from a peer and ends up as rustup arguments and a
        // download: nothing unvalidated reaches that far.
        if let Err(e) = validate_request(id, target) {
            tracing::warn!("toolchain request refused: {e}");
            return None;
        }
        let key = (spec.clone(), target.to_string());
        if st.failed.contains(&key) {
            return None;
        }
        if found.is_none() && st.auto.len() >= self.inner.max_auto {
            tracing::warn!(
                "toolchain {spec} not installed: {} auto-installed toolchains reach max_auto_toolchains",
                st.auto.len()
            );
            return None;
        }
        st.installing = Some(InstallState { pid: None });
        drop(st);

        let this = self.clone();
        let id = id.clone();
        let target = target.to_string();
        let spawned = std::thread::Builder::new()
            .name("crabfarm-install".into())
            .spawn(move || {
                let ok = match this.install_blocking(&id, &spec, &target) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!("toolchain install {spec} (target {target}) failed: {e:#}");
                        false
                    }
                };
                let mut st = this.lock();
                st.installing = None;
                if !ok {
                    st.failed.insert((spec, target));
                }
            });
        if spawned.is_err() {
            let mut st = self.lock();
            st.installing = None;
        }
        None
    }

    /// Install steps, run on the background thread.
    fn install_blocking(&self, id: &ToolchainId, spec: &str, target: &str) -> anyhow::Result<()> {
        let mut found = self.list().into_iter().find(|t| t.id.same_compiler(id));
        if found.is_none() {
            tracing::info!("installing toolchain {spec}");
            let before: Vec<String> = self.list().into_iter().map(|t| t.name).collect();
            let mut cmd = Command::new("rustup");
            cmd.args([
                "toolchain",
                "install",
                spec,
                "--profile",
                "minimal",
                "--no-self-update",
            ]);
            let install_res = self.run_recorded(cmd, INSTALL_TIMEOUT);
            // Even a failed or timed out install may have left a toolchain.
            let rescan_res = self.rescan();
            let added = newly_added(&before, &self.list());
            if !added.is_empty() {
                let mut st = self.lock();
                for n in &added {
                    if !st.auto.contains(n) {
                        st.auto.push(n.clone());
                    }
                }
                let names = st.auto.clone();
                drop(st);
                self.save_auto(&names);
            }
            install_res?;
            rescan_res?;
            found = self.list().into_iter().find(|t| t.id.same_compiler(id));
            if found.is_none() {
                // The spec resolved to a different build (for example a newer
                // nightly): never use it for this job, and do not keep it.
                self.uninstall_auto(&added);
                bail!(
                    "toolchain {spec} installed but commit {} was not found; not used",
                    short(&id.commit_hash)
                );
            }
        }
        let Some(t) = found else {
            bail!("toolchain vanished")
        };
        if !t.targets.iter().any(|x| x == target) {
            tracing::info!("adding target {target} to {}", t.name);
            let mut cmd = Command::new("rustup");
            cmd.args(["target", "add", "--toolchain", &t.name, target]);
            self.run_recorded(cmd, INSTALL_TIMEOUT)?;
            self.rescan()?;
            let ok = self
                .list()
                .iter()
                .any(|x| x.id.same_compiler(id) && x.targets.iter().any(|y| y == target));
            if !ok {
                bail!("target {target} still missing after rustup target add");
            }
        }
        Ok(())
    }

    /// Run one rustup child with the install timeout, recording its PID so
    /// that only this process can be killed.
    fn run_recorded(&self, mut cmd: Command, timeout: Duration) -> anyhow::Result<()> {
        crate::proctree::no_window(&mut cmd);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().context("spawn rustup")?;
        if let Some(i) = self.lock().installing.as_mut() {
            i.pid = Some(child.id());
        }
        let res = wait_with_timeout(&mut child, timeout);
        if let Some(i) = self.lock().installing.as_mut() {
            i.pid = None;
        }
        let status = res?;
        if !status.success() {
            bail!("rustup exited with {status}");
        }
        Ok(())
    }

    /// Remove toolchains this node itself just installed (`names` come from
    /// [`newly_added`]); never touches anything else. Failures are logged.
    fn uninstall_auto(&self, names: &[String]) {
        for name in names {
            let mut cmd = Command::new("rustup");
            cmd.args(["toolchain", "uninstall", name]);
            if let Err(e) = self.run_recorded(cmd, UNINSTALL_TIMEOUT) {
                tracing::warn!("could not uninstall {name}: {e:#}");
            }
        }
        if let Err(e) = self.rescan() {
            tracing::warn!("rescan after uninstall failed: {e:#}");
        }
        let names = self.lock().auto.clone();
        self.save_auto(&names);
    }

    /// rustup spec for an id: `release` for stable/beta, `nightly-<commit
    /// date + 1 day>` for nightly; install is verified by commit hash.
    pub fn install_spec(id: &ToolchainId) -> String {
        if id.release.contains("-nightly") {
            if let Some(d) = next_day(&id.commit_date) {
                return format!("nightly-{d}");
            }
            return "nightly".to_string();
        }
        id.release.clone()
    }
}

/// Names present in `after` but not in `before`: what one install added.
fn newly_added(before: &[String], after: &[Installed]) -> Vec<String> {
    after
        .iter()
        .filter(|t| !before.contains(&t.name))
        .map(|t| t.name.clone())
        .collect()
}

fn load_auto(data_dir: &Path, installed: &[Installed]) -> Vec<String> {
    #[derive(Deserialize, Default)]
    struct F {
        #[serde(default)]
        names: Vec<String>,
    }
    let f: F = std::fs::read_to_string(data_dir.join(AUTO_FILE))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    f.names
        .into_iter()
        .filter(|n| installed.iter().any(|t| t.name == *n))
        .collect()
}

fn write_auto(data_dir: &Path, names: &[String]) -> anyhow::Result<()> {
    #[derive(Serialize)]
    struct F<'a> {
        names: &'a [String],
    }
    let text = toml::to_string(&F { names }).context("encode auto toolchains")?;
    std::fs::create_dir_all(data_dir).context("create data dir")?;
    let tmp = data_dir.join(format!("{AUTO_FILE}.tmp"));
    std::fs::write(&tmp, text).context("write auto toolchains")?;
    std::fs::rename(&tmp, data_dir.join(AUTO_FILE)).context("replace auto toolchains")
}

// ---- validation of peer-controlled specs ----

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `^\d+\.\d+\.\d+(-beta\.\d+|-nightly)?$`
pub fn valid_release(s: &str) -> bool {
    let (core, suffix) = match s.split_once('-') {
        Some((c, rest)) => (c, Some(rest)),
        None => (s, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 || !parts.iter().all(|p| digits(p)) {
        return false;
    }
    match suffix {
        None | Some("nightly") => true,
        Some(r) => r.strip_prefix("beta.").is_some_and(digits),
    }
}

/// `YYYY-MM-DD` and a date that exists.
pub fn valid_commit_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let (ys, ms, ds) = (&s[0..4], &s[5..7], &s[8..10]);
    if !(digits(ys) && digits(ms) && digits(ds)) {
        return false;
    }
    let (Ok(y), Ok(m), Ok(d)) = (ys.parse::<u32>(), ms.parse::<u32>(), ds.parse::<u32>()) else {
        return false;
    };
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    y >= 2000 && (1..=dim).contains(&d)
}

/// 40 lowercase hex characters.
pub fn valid_commit_hash(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `^[A-Za-z0-9_.-]{1,64}$`, not starting with `-` (it becomes a rustup argument).
pub fn valid_target(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Check everything a peer-supplied id and target contribute to a rustup call.
pub fn validate_request(id: &ToolchainId, target: &str) -> Result<(), &'static str> {
    if !valid_release(&id.release) {
        return Err("invalid release");
    }
    if !valid_commit_date(&id.commit_date) {
        return Err("invalid commit date");
    }
    if !valid_commit_hash(&id.commit_hash) {
        return Err("invalid commit hash");
    }
    if !valid_target(target) {
        return Err("invalid target");
    }
    Ok(())
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(10)]
}

/// `YYYY-MM-DD` plus one day, `None` when the input is not such a date.
fn next_day(date: &str) -> Option<String> {
    let mut it = date.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days-from-civil / civil-from-days (proleptic Gregorian).
    let (yy, mm) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * mm + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468 + 1;

    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let nd = doy - (153 * mp + 2) / 5 + 1;
    let nm = if mp < 10 { mp + 3 } else { mp - 9 };
    let ny = yoe + era * 400 + i64::from(nm <= 2);
    Some(format!("{ny:04}-{nm:02}-{nd:02}"))
}

// ---- process helpers ----

/// Wait for `child` up to `timeout`; on timeout kill exactly this child.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> anyhow::Result<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(s) = child.try_wait().context("wait for child")? {
            return Ok(s);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("child timed out after {} s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Run a command, return stdout as text; stderr is discarded.
fn run_capture(mut cmd: Command, timeout: Duration) -> anyhow::Result<String> {
    crate::proctree::no_window(&mut cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().context("spawn")?;
    let mut stdout = child.stdout.take().context("no stdout pipe")?;
    // Reader thread so a full pipe cannot stall the child while we poll.
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let status = wait_with_timeout(&mut child, timeout)?;
    let buf = reader.join().unwrap_or_default();
    if !status.success() {
        bail!("exited with {status}");
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Run the kill helper with a timeout (the helper itself can hang).
fn run_kill_helper(mut cmd: Command) {
    crate::proctree::no_window(&mut cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            if let Err(e) = wait_with_timeout(&mut child, KILL_TIMEOUT) {
                tracing::warn!("kill helper: {e:#}");
            }
        }
        Err(e) => tracing::warn!("kill helper did not start: {e}"),
    }
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    // Only called with a PID this module spawned and recorded.
    let mut cmd = Command::new("taskkill");
    cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
    run_kill_helper(cmd);
}

#[cfg(not(windows))]
fn kill_pid(pid: u32) {
    let mut cmd = Command::new("kill");
    cmd.args(["-KILL", &pid.to_string()]);
    run_kill_helper(cmd);
}

// ---- scanning and the on-disk cache ----

#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    #[serde(default)]
    toolchain: Vec<CacheEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
struct CacheEntry {
    name: String,
    /// Stamp of the toolchain directory (see `stamp`).
    stamp: String,
    release: String,
    commit_hash: String,
    commit_date: String,
    host: String,
    targets: Vec<String>,
}

fn rustup_home() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("RUSTUP_HOME")
        && !p.is_empty()
    {
        return Ok(PathBuf::from(p));
    }
    let base = directories::BaseDirs::new().context("no home directory")?;
    Ok(base.home_dir().join(".rustup"))
}

fn mtime_nanos(p: &Path) -> u128 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Cache key of a toolchain. The brief keys on the toolchain dir mtime; that
/// does not change when `rustup target add` creates `lib/rustlib/<triple>`, so
/// the mtime of `lib/rustlib` is part of the stamp too (otherwise the cached
/// target list would go stale).
fn stamp(dir: &Path) -> String {
    format!(
        "{}:{}",
        mtime_nanos(dir),
        mtime_nanos(&dir.join("lib").join("rustlib"))
    )
}

fn parse_toolchain_list(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.split_whitespace().next())
        // `rustup toolchain list` prints a sentence when nothing is installed.
        .filter(|n| !n.eq_ignore_ascii_case("no") && !n.contains(':'))
        .map(str::to_string)
        .collect()
}

fn scan_installed(data_dir: &Path) -> anyhow::Result<Vec<Installed>> {
    let mut cmd = Command::new("rustup");
    cmd.args(["toolchain", "list"]);
    let list = run_capture(cmd, QUERY_TIMEOUT).context("rustup toolchain list")?;
    let names = parse_toolchain_list(&list);
    let home = rustup_home()?;

    let cache_path = data_dir.join("toolchains.toml");
    let old: CacheFile = std::fs::read_to_string(&cache_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();

    let exe = if cfg!(windows) { "rustc.exe" } else { "rustc" };
    let mut out = Vec::new();
    let mut entries = Vec::new();
    for name in names {
        let dir = home.join("toolchains").join(&name);
        let rustc = dir.join("bin").join(exe);
        if !rustc.is_file() {
            continue;
        }
        let st = stamp(&dir);
        let cached = old
            .toolchain
            .iter()
            .find(|e| e.name == name && e.stamp == st)
            .cloned();
        let entry = match cached {
            Some(e) => e,
            None => match fresh_entry(&name, &rustc, st) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("toolchain {name} skipped: {e:#}");
                    continue;
                }
            },
        };
        out.push(Installed {
            id: ToolchainId {
                release: entry.release.clone(),
                commit_hash: entry.commit_hash.clone(),
                commit_date: entry.commit_date.clone(),
                host: entry.host.clone(),
            },
            name: name.clone(),
            rustc,
            targets: entry.targets.clone(),
        });
        entries.push(entry);
    }

    // Best effort: a cache that cannot be written only costs time next start.
    if let Ok(text) = toml::to_string(&CacheFile { toolchain: entries }) {
        let tmp = data_dir.join("toolchains.toml.tmp");
        if std::fs::create_dir_all(data_dir).is_ok()
            && std::fs::write(&tmp, text).is_ok()
            && std::fs::rename(&tmp, &cache_path).is_err()
        {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    Ok(out)
}

fn fresh_entry(name: &str, rustc: &Path, stamp: String) -> anyhow::Result<CacheEntry> {
    let id = query(rustc)?;
    let mut cmd = Command::new("rustup");
    cmd.args(["target", "list", "--installed", "--toolchain", name]);
    let list = run_capture(cmd, QUERY_TIMEOUT).context("rustup target list")?;
    let targets = list
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.split_whitespace().next().unwrap_or(l).to_string())
        .collect();
    Ok(CacheEntry {
        name: name.to_string(),
        stamp,
        release: id.release,
        commit_hash: id.commit_hash,
        commit_date: id.commit_date,
        host: id.host,
        targets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STABLE: &str = "rustc 1.100.0 (0ef1bc386 2026-11-09)\nbinary: rustc\ncommit-hash: 0ef1bc386059596dada8fb65b50425aab2ea637a\ncommit-date: 2026-11-09\nhost: x86_64-pc-windows-msvc\nrelease: 1.100.0\nLLVM version: 23.1.5\n";
    const BETA: &str = "rustc 1.101.0-beta.3 (aaaabbbb1 2026-11-12)\r\nbinary: rustc\r\ncommit-hash: aaaabbbb1111222233334444555566667777888\r\ncommit-date: 2026-11-12\r\nhost: aarch64-apple-darwin\r\nrelease: 1.101.0-beta.3\r\nLLVM version: 23.1.6\r\n";
    const NIGHTLY: &str = "rustc 1.102.0-nightly (ccccdddd2 2026-12-31)\nbinary: rustc\ncommit-hash: ccccdddd2222333344445555666677778888999\ncommit-date: 2026-12-31\nhost: x86_64-unknown-linux-gnu\nrelease: 1.102.0-nightly\nLLVM version: 23.1.7\n";

    #[test]
    fn parses_stable_beta_nightly() {
        let s = parse_version_verbose(STABLE).unwrap();
        assert_eq!(s.release, "1.100.0");
        assert_eq!(s.commit_hash, "0ef1bc386059596dada8fb65b50425aab2ea637a");
        assert_eq!(s.commit_date, "2026-11-09");
        assert_eq!(s.host, "x86_64-pc-windows-msvc");
        let b = parse_version_verbose(BETA).unwrap();
        assert_eq!(b.release, "1.101.0-beta.3");
        assert_eq!(b.host, "aarch64-apple-darwin");
        let n = parse_version_verbose(NIGHTLY).unwrap();
        assert_eq!(n.release, "1.102.0-nightly");
    }

    #[test]
    fn rejects_incomplete_or_unknown_hash() {
        assert!(parse_version_verbose("release: 1.0.0\nhost: x\n").is_err());
        assert!(parse_version_verbose("release: 1.0.0\ncommit-hash: unknown\nhost: x\n").is_err());
        assert!(parse_version_verbose("").is_err());
    }

    #[test]
    fn install_specs() {
        assert_eq!(
            Toolchains::install_spec(&parse_version_verbose(STABLE).unwrap()),
            "1.100.0"
        );
        assert_eq!(
            Toolchains::install_spec(&parse_version_verbose(BETA).unwrap()),
            "1.101.0-beta.3"
        );
        // Year end rolls over: commit date + 1 day.
        assert_eq!(
            Toolchains::install_spec(&parse_version_verbose(NIGHTLY).unwrap()),
            "nightly-2027-01-01"
        );
    }

    #[test]
    fn next_day_edges() {
        assert_eq!(next_day("2026-11-09").as_deref(), Some("2026-11-10"));
        assert_eq!(next_day("2026-11-30").as_deref(), Some("2026-12-01"));
        assert_eq!(next_day("2028-02-28").as_deref(), Some("2028-02-29"));
        assert_eq!(next_day("2028-02-29").as_deref(), Some("2028-03-01"));
        assert_eq!(next_day("2027-02-28").as_deref(), Some("2027-03-01"));
        assert_eq!(next_day("garbage"), None);
        assert_eq!(next_day("2026-13-01"), None);
    }

    #[test]
    fn toolchain_list_parsing() {
        let out = "stable-x86_64-pc-windows-msvc (default)\n1.95.0-x86_64-pc-windows-msvc\nnightly-2026-11-01-x86_64-pc-windows-msvc (active)\n";
        assert_eq!(
            parse_toolchain_list(out),
            vec![
                "stable-x86_64-pc-windows-msvc",
                "1.95.0-x86_64-pc-windows-msvc",
                "nightly-2026-11-01-x86_64-pc-windows-msvc"
            ]
        );
        assert!(parse_toolchain_list("no installed toolchains\n").is_empty());
    }

    fn fake(release: &str, hash: &str, targets: &[&str]) -> Installed {
        Installed {
            id: ToolchainId {
                release: release.into(),
                commit_hash: hash.into(),
                commit_date: "2026-11-09".into(),
                host: "h".into(),
            },
            name: release.into(),
            rustc: PathBuf::from("rustc"),
            targets: targets.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn resolve_matches_compiler_and_target_without_install() {
        let d = tempfile::tempdir().unwrap();
        let tc = Toolchains::with_installed(d.path(), false, vec![fake("1.100.0", "abc", &["h"])]);
        let want = ToolchainId {
            release: "1.100.0".into(),
            commit_hash: "abc".into(),
            commit_date: String::new(),
            host: "other".into(),
        };
        assert!(
            tc.resolve(&want, "h").is_some(),
            "host of the id may differ"
        );
        assert!(
            tc.resolve(&want, "wasm32").is_none(),
            "missing target, auto_install off"
        );
        let other = ToolchainId {
            commit_hash: "zzz".into(),
            ..want
        };
        assert!(
            tc.resolve(&other, "h").is_none(),
            "same release, other commit"
        );
        assert!(!tc.is_installing());
    }

    const HASH: &str = "0ef1bc386059596dada8fb65b50425aab2ea637a";

    fn wanted(release: &str, hash: &str, date: &str) -> ToolchainId {
        ToolchainId {
            release: release.into(),
            commit_hash: hash.into(),
            commit_date: date.into(),
            host: "h".into(),
        }
    }

    #[test]
    fn release_validation() {
        for ok in ["1.100.0", "1.101.0-beta.3", "1.102.0-nightly", "0.0.0"] {
            assert!(valid_release(ok), "{ok}");
        }
        for bad in [
            "",
            "-1.0.0",
            "--help",
            "1.100",
            "1.100.0.1",
            "1.100.0-beta",
            "1.100.0-beta.",
            "1.100.0-rc.1",
            "stable",
            "nightly",
            "1.100.0 --x",
            "1.100.0-nightly.1",
            "a.b.c",
            "1.100.0\n",
        ] {
            assert!(!valid_release(bad), "{bad:?}");
        }
    }

    #[test]
    fn date_validation() {
        assert!(valid_commit_date("2026-11-09"));
        assert!(valid_commit_date("2028-02-29"));
        for bad in [
            "",
            "2027-02-29",
            "2026-13-01",
            "2026-00-10",
            "2026-11-31",
            "2026-1-09",
            "26-11-09",
            "-026-11-09",
            "2026-11-09x",
            "2026/11/09",
            "1999-01-01",
        ] {
            assert!(!valid_commit_date(bad), "{bad:?}");
        }
    }

    #[test]
    fn hash_and_target_validation() {
        assert!(valid_commit_hash(HASH));
        assert!(!valid_commit_hash(&HASH.to_uppercase()));
        assert!(!valid_commit_hash(&HASH[..39]));
        assert!(!valid_commit_hash(&format!("{HASH}0")));
        assert!(!valid_commit_hash(&format!("{}g", &HASH[..39])));
        assert!(valid_target("x86_64-pc-windows-msvc"));
        assert!(valid_target("wasm32-unknown-unknown"));
        for bad in ["", "-v", "--help", "a b", "a/b", "a;b", &"x".repeat(65)] {
            assert!(!valid_target(bad), "{bad:?}");
        }
        assert!(validate_request(&wanted("1.100.0", HASH, "2026-11-09"), "h").is_ok());
        assert!(validate_request(&wanted("-v", HASH, "2026-11-09"), "h").is_err());
        assert!(validate_request(&wanted("1.100.0", HASH, "x"), "h").is_err());
        assert!(validate_request(&wanted("1.100.0", "abc", "2026-11-09"), "h").is_err());
        assert!(validate_request(&wanted("1.100.0", HASH, "2026-11-09"), "--bad").is_err());
    }

    #[test]
    fn invalid_request_never_starts_an_install() {
        let d = tempfile::tempdir().unwrap();
        let tc = Toolchains::with_installed(d.path(), true, vec![]);
        // `--bogus-flag` would be a rustup argument on unvalidated code.
        assert!(
            tc.resolve(&wanted("--bogus-flag", HASH, "2026-11-09"), "h")
                .is_none()
        );
        assert!(
            !tc.is_installing(),
            "an invalid release must not start rustup"
        );
        assert!(
            tc.resolve(&wanted("9.99.9", HASH, "2026-11-09"), "--bogus-flag")
                .is_none()
        );
        assert!(
            !tc.is_installing(),
            "an invalid target must not start rustup"
        );
        assert!(
            tc.resolve(&wanted("9.99.9", "deadbeef", "2026-11-09"), "h")
                .is_none()
        );
        assert!(
            !tc.is_installing(),
            "an invalid commit hash must not start rustup"
        );
    }

    #[test]
    fn auto_install_cap_blocks_new_toolchains() {
        let d = tempfile::tempdir().unwrap();
        let mut a = fake("1.90.0", "abc", &["h"]);
        a.name = "1.90.0-h".into();
        let mut b = fake("1.91.0", "abd", &["h"]);
        b.name = "1.91.0-h".into();
        write_auto(
            d.path(),
            &[
                "1.90.0-h".to_string(),
                "1.91.0-h".to_string(),
                "gone-h".to_string(),
            ],
        )
        .unwrap();
        let tc = Toolchains::with_installed_cap(d.path(), true, 2, vec![a, b]);
        // The entry for a toolchain that no longer exists does not count.
        assert_eq!(tc.auto_installed(), vec!["1.90.0-h", "1.91.0-h"]);
        let new = wanted("9.99.9", HASH, "2026-11-09");
        assert!(tc.resolve(&new, "h").is_none());
        assert!(
            !tc.is_installing(),
            "cap reached: no new toolchain may be installed"
        );
        // Cap 0 disables toolchain auto installs entirely.
        let tc0 = Toolchains::with_installed_cap(d.path(), true, 0, vec![]);
        assert!(tc0.resolve(&new, "h").is_none());
        assert!(!tc0.is_installing());
    }

    #[test]
    fn failed_set_is_bounded_lru() {
        let mut f = FailedSet::default();
        let k = |i: usize| (format!("spec{i}"), "t".to_string());
        for i in 0..FAILED_CAP {
            f.insert(k(i));
        }
        assert!(f.contains(&k(0)), "refreshes entry 0");
        f.insert(k(1000));
        assert_eq!(f.order.len(), FAILED_CAP);
        assert!(f.contains(&k(0)), "recently used survives");
        assert!(!f.contains(&k(1)), "least recently used is evicted");
        for i in 2000..2500 {
            f.insert(k(i));
        }
        assert_eq!(f.order.len(), FAILED_CAP);
    }

    #[test]
    fn only_newly_added_toolchains_are_removable() {
        let before = vec!["stable-h".to_string(), "nightly-2026-11-01-h".to_string()];
        let mut n = fake("1.1.1", "x", &[]);
        n.name = "nightly-2026-11-10-h".into();
        let mut s = fake("1.1.1", "y", &[]);
        s.name = "stable-h".into();
        assert_eq!(newly_added(&before, &[s, n]), vec!["nightly-2026-11-10-h"]);
    }

    #[test]
    fn scan_lists_active_toolchain() {
        // Read-only against the machine's real rustup.
        let d = tempfile::tempdir().unwrap();
        let tc = Toolchains::scan(d.path(), false).unwrap();
        let active = query(Path::new("rustc")).unwrap();
        let list = tc.list();
        let hit = list
            .iter()
            .find(|t| t.id.same_compiler(&active))
            .expect("active toolchain not listed");
        assert!(hit.rustc.is_file());
        assert!(
            hit.targets.contains(&active.host),
            "host target missing: {:?}",
            hit.targets
        );
        assert!(tc.resolve(&active, &active.host).is_some());
        // Second scan is served from the cache file and agrees.
        assert!(d.path().join("toolchains.toml").is_file());
        let again = Toolchains::scan(d.path(), false).unwrap();
        assert_eq!(again.list().len(), list.len());
    }
}
