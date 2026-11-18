//! Package C. Run one job on the executing peer (DESIGN section 9).
//!
//! Transport-agnostic: the caller (node, package D) moves frames; this module
//! materializes the sandbox, runs rustc under the node jobserver, and emits
//! [`ExecEvent`]s in the order the submitter must apply them.

use crate::cas::BlobStore;
use crate::guard;
use crate::paths::{SandboxMap, host_style};
use crate::proto::{Arg, BlobHash, ClientPath, EnvValue, JobId, JobSpec, NodeId};
use crate::rewrite::{self, Rewriter};
use crate::rustc_args::render_args;
use crate::toolchain::Installed;
use anyhow::{Context, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc;

/// Node-wide jobserver shared by all rustc processes on this peer.
pub struct NodeJobserver {
    pub client: jobserver::Client,
}

impl NodeJobserver {
    pub fn new(slots: u32) -> anyhow::Result<NodeJobserver> {
        let client = jobserver::Client::new(slots.max(1) as usize).context("create jobserver")?;
        Ok(NodeJobserver { client })
    }
}

/// Events for the submitter, in order.
#[derive(Debug)]
pub enum ExecEvent {
    Started,
    /// Already rewritten to client paths.
    Stderr(String),
    Stdout(String),
    /// A produced file: the plaintext is stored in the blob store under the
    /// submitter's key (unless `spec.no_retain`) and also returned here.
    /// `.d` files are already rewritten. For an artifact notification line
    /// the `Output` is emitted BEFORE the corresponding `Stderr` line.
    Output {
        path: ClientPath,
        hash: BlobHash,
        data: Vec<u8>,
    },
    /// The output `path` contains this peer's sandbox path (CF-10). Sent once,
    /// instead of that output; the job is stopped right after and no `Exit` follows.
    Tainted(ClientPath),
    Exit(i32),
}

/// Why `run` ended without an `Exit` event. Carried inside the `anyhow` error
/// so the node can tell a deadline or a cancel from an infrastructure failure
/// (`err.downcast_ref::<ExecAborted>()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExecAborted {
    #[error("job timed out")]
    Timeout,
    #[error("job cancelled")]
    Cancelled,
    /// An output embeds the sandbox path; `ExecEvent::Tainted` was sent.
    #[error("output embeds the sandbox path")]
    Tainted,
}

pub struct ExecContext<'a> {
    /// `<data>/jobs`
    pub jobs_dir: PathBuf,
    pub store: &'a BlobStore,
    pub jobserver: &'a NodeJobserver,
    /// Submitter node id and blob key (for reading inputs and storing outputs).
    pub owner: NodeId,
    pub key: [u8; 32],
    pub timeout: std::time::Duration,
    /// Config `accept_env` of this node (CF-19).
    pub accept_env: &'a [String],
}

/// Inputs from `spec.inputs` that are not in the store for `owner`.
pub fn missing_inputs(ctx: &ExecContext, spec: &JobSpec) -> Vec<BlobHash> {
    let mut seen = std::collections::HashSet::new();
    spec.inputs
        .iter()
        .filter(|i| !ctx.store.has(ctx.owner, &i.hash))
        .filter(|i| seen.insert(i.hash))
        .map(|i| i.hash)
        .collect()
}

/// Directory name of a job: the submitter's node id and the job id (CF-9).
/// The job id alone is chosen by the submitter, so two submitters (or a
/// malicious one) could otherwise pick the same name and wipe a running job.
fn job_hex(owner: NodeId, id: &JobId) -> String {
    let hex = |b: &[u8; 16]| b.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!("{}-{}", hex(&owner.0), hex(&id.0))
}

/// Jobs running in this process, by `(jobs dir, owner, job id)`: a second
/// request for the same key is refused instead of deleting the first one's
/// sandbox (CF-9).
fn running_jobs() -> &'static std::sync::Mutex<std::collections::HashSet<(PathBuf, NodeId, JobId)>>
{
    static RUNNING: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashSet<(PathBuf, NodeId, JobId)>>,
    > = std::sync::OnceLock::new();
    RUNNING.get_or_init(Default::default)
}

struct RunningGuard((PathBuf, NodeId, JobId));

impl RunningGuard {
    fn claim(ctx: &ExecContext, spec: &JobSpec) -> anyhow::Result<RunningGuard> {
        let key = (ctx.jobs_dir.clone(), ctx.owner, spec.job_id);
        let mut set = running_jobs().lock().unwrap_or_else(|e| e.into_inner());
        if !set.insert(key.clone()) {
            bail!("job is already running on this node");
        }
        Ok(RunningGuard(key))
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        running_jobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// Build-related variables the node must not leak into a job: the job gets
/// its own only from the submitter (`spec.env`). System variables (PATH,
/// SystemRoot, LIB, INCLUDE, ...) stay because rustc and the MSVC linker need them.
fn is_build_var(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    n.starts_with("CARGO")
        || n.starts_with("RUSTC")
        || n.starts_with("RUSTDOC")
        || n.starts_with("DEP_")
        || matches!(
            n.as_str(),
            "RUSTFLAGS"
                | "OUT_DIR"
                | "TARGET"
                | "HOST"
                | "PROFILE"
                | "NUM_JOBS"
                | "OPT_LEVEL"
                | "DEBUG"
                | "MAKEFLAGS"
                | "MFLAGS"
        )
}

/// Every client path a spec mentions.
fn spec_paths(spec: &JobSpec) -> Vec<&ClientPath> {
    let mut v: Vec<&ClientPath> = Vec::new();
    v.extend(spec.inputs.iter().map(|i| &i.path));
    v.extend(spec.dirs.iter());
    v.push(&spec.out_dir);
    v.push(&spec.cwd);
    for a in &spec.args {
        match a {
            Arg::Plain(_) => {}
            Arg::Path(p) => v.push(p),
            Arg::PrefixedPath { path, .. } => v.push(path),
            Arg::RemapFrom { from, .. } => v.push(from),
        }
    }
    for (_, e) in &spec.env {
        if let EnvValue::Path(p) = e {
            v.push(p);
        }
    }
    v
}

/// A peer must not be able to place files outside the sandbox with a hand-built
/// path (`..` components, `D:` or `\` inside a component): every path has to be
/// what `ClientPath::parse` yields (which checks each component) and must map
/// below the sandbox root (CF-7).
fn validate_paths(spec: &JobSpec, map: &SandboxMap) -> anyhow::Result<()> {
    for p in spec_paths(spec) {
        if ClientPath::parse(p.style, &p.path).as_ref() != Some(p) {
            bail!("job contains a path that is not absolute and normalized");
        }
        sandbox_path(map, p)?;
    }
    Ok(())
}

/// Whether the file system under `jobs_dir` ignores case, so that two sandbox
/// names that differ only in case (or Unicode normalization) are one file
/// (CF-19). Windows and macOS are assumed insensitive (their defaults). Elsewhere
/// the directory is probed once per process (`Aa` created, `aa` looked up); a
/// probe that fails counts as insensitive, which only makes the check stricter.
fn sandbox_case_insensitive(jobs_dir: &Path) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        return true;
    }
    static PROBED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PROBED.get_or_init(|| {
        let mut tag = [0u8; 4];
        rand::fill(&mut tag[..]);
        let hex: String = tag.iter().map(|b| format!("{b:02x}")).collect();
        let upper = jobs_dir.join(format!(".case-probe-{hex}-Aa"));
        let lower = jobs_dir.join(format!(".case-probe-{hex}-aa"));
        if std::fs::create_dir_all(jobs_dir).is_err() || std::fs::write(&upper, b"").is_err() {
            return true;
        }
        let insensitive = lower.exists();
        let _ = std::fs::remove_file(&upper);
        insensitive
    })
}

/// Refuse a job whose argument list, environment or size is outside what the
/// submitter's wrapper can produce (CF-8). The submitter then runs it locally.
pub fn validate_spec(
    spec: &JobSpec,
    accept_env: &[String],
    peer_host: &str,
    jobs_dir: &Path,
) -> anyhow::Result<()> {
    let node_has = |n: &str| std::env::var_os(n).is_some();
    let policy = guard::Policy {
        peer_host,
        accept_env,
        node_has: &node_has,
        case_insensitive: sandbox_case_insensitive(jobs_dir),
    };
    guard::validate_spec(spec, &policy).map_err(|why| anyhow::anyhow!("job refused: {why}"))
}

/// `SandboxMap::to_sandbox` with an error for a path that cannot be mapped.
fn sandbox_path(map: &SandboxMap, p: &ClientPath) -> anyhow::Result<PathBuf> {
    map.to_sandbox(p)
        .context("path cannot be mapped into the sandbox")
}

/// Background removal: first retry after `BASE`, doubling up to `CAP`, at most
/// `ATTEMPTS` tries (about ten minutes with the defaults).
const CLEANUP_ATTEMPTS: u32 = 20;
const CLEANUP_BASE: std::time::Duration = std::time::Duration::from_secs(1);
const CLEANUP_CAP: std::time::Duration = std::time::Duration::from_secs(30);

/// Try to remove `dir` until it is gone; `true` when it is. Logs the first
/// failure and the end, never file names inside the directory.
fn retry_remove(
    dir: &Path,
    attempts: u32,
    base: std::time::Duration,
    cap: std::time::Duration,
) -> bool {
    let mut delay = base;
    for attempt in 0..attempts {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => {
                if attempt > 0 {
                    tracing::info!(
                        "job directory {} removed after {attempt} retries",
                        dir.display()
                    );
                }
                return true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
            Err(e) => {
                if attempt == 0 {
                    tracing::warn!(
                        "could not delete job directory {} yet: {e}; retrying in the background",
                        dir.display()
                    );
                }
            }
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(cap);
    }
    let gone = std::fs::remove_dir_all(dir).is_ok() || !dir.exists();
    if !gone {
        tracing::error!(
            "job directory {} still exists after {attempts} attempts; it holds job sources and must be deleted by hand",
            dir.display()
        );
    }
    gone
}

/// Retry the removal of `dir` on a thread of its own, bounded and logged. A
/// directory that cannot be removed holds plaintext sources of another
/// machine, so giving up after one second is not acceptable (CF-9).
pub fn spawn_cleanup(dir: PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("crabfarm-cleanup".into())
        .spawn({
            let dir = dir.clone();
            move || {
                retry_remove(&dir, CLEANUP_ATTEMPTS, CLEANUP_BASE, CLEANUP_CAP);
            }
        });
    if let Err(e) = spawned {
        tracing::error!("could not start the cleanup of {}: {e}", dir.display());
    }
}

/// Deletes the job directory when dropped. The normal path calls
/// `remove_job_dir` explicitly; this covers a dropped future.
struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => spawn_cleanup(self.0.clone()),
        }
    }
}

/// Remove the job directory, retrying briefly: on Windows a file stays locked
/// for a moment after its process exited (virus scanner, handle teardown).
/// Whatever is still there after that goes to the background cleanup.
async fn remove_job_dir(dir: &Path) {
    for _ in 0..10 {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    }
    spawn_cleanup(dir.to_path_buf());
}

/// Delete everything below `<data>/jobs`: sandboxes a crashed node left
/// behind. Directories that cannot be removed yet are retried in the
/// background. Call at node start, before any job runs.
pub fn sweep_jobs(jobs_dir: &Path) {
    let Ok(rd) = std::fs::read_dir(jobs_dir) else {
        return;
    };
    for e in rd.flatten() {
        let path = e.path();
        let removed = if e.file_type().is_ok_and(|t| t.is_dir()) {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if removed.is_err() && path.exists() {
            spawn_cleanup(path);
        }
    }
}

/// Linker programs a job may name by absolute path (the submitter's own
/// toolchain layout); anything else is dropped.
fn known_linker(path: &Path) -> bool {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        stem.as_str(),
        "link"
            | "lld-link"
            | "rust-lld"
            | "ld"
            | "ld.lld"
            | "lld"
            | "cc"
            | "gcc"
            | "g++"
            | "c++"
            | "clang"
            | "clang++"
    ) || ["-gcc", "-g++", "-clang", "-cc"]
        .iter()
        .any(|s| stem.ends_with(s))
}

fn under_dir(p: &Path, dir: &Path) -> bool {
    let norm = |p: &Path| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .to_ascii_lowercase()
            .replace('\\', "/")
    };
    norm(p).starts_with(&norm(dir))
}

/// The submitter's `-C linker=<absolute path>` names a file of ITS machine.
/// Keep it (as that native path, not a sandbox path) only when this peer has
/// the same host triple and a known linker program at exactly that path,
/// outside the job sandboxes; otherwise drop the flag and let rustc use its
/// default linker (rustc_args docs, DESIGN section 5).
fn linker_on_this_peer(
    spec: &JobSpec,
    toolchain: &Installed,
    jobs_dir: &Path,
    p: &ClientPath,
) -> Option<String> {
    if spec.toolchain.host != toolchain.id.host || p.style != host_style() {
        return None;
    }
    let native = Path::new(&p.path);
    (native.is_file() && known_linker(native) && !under_dir(native, jobs_dir))
        .then(|| p.path.clone())
}

/// `render_args` with the sandbox mapping, except for `-C linker=<path>`.
fn render_job_args(
    spec: &JobSpec,
    map: &SandboxMap,
    toolchain: &Installed,
    jobs_dir: &Path,
) -> Vec<String> {
    let args = &spec.args;
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        match (&args[i], args.get(i + 1)) {
            // `-C linker=<path>`: flag and value are separate arguments.
            (Arg::Plain(flag), Some(Arg::PrefixedPath { prefix, path }))
                if (flag == "-C" || flag == "--codegen") && prefix == "linker=" =>
            {
                match linker_on_this_peer(spec, toolchain, jobs_dir, path) {
                    Some(native) => {
                        out.push(flag.clone());
                        out.push(format!("linker={native}"));
                    }
                    None => {
                        tracing::debug!(
                            crate_name = crate::proto::log_safe(&spec.crate_name).as_str(),
                            "dropped -C linker"
                        )
                    }
                }
                i += 2;
            }
            (Arg::PrefixedPath { prefix, path }, _) if prefix.ends_with("linker=") => {
                match linker_on_this_peer(spec, toolchain, jobs_dir, path) {
                    Some(native) => out.push(format!("{prefix}{native}")),
                    None => {
                        tracing::debug!(
                            crate_name = crate::proto::log_safe(&spec.crate_name).as_str(),
                            "dropped -C linker"
                        )
                    }
                }
                i += 1;
            }
            (a, _) => {
                out.extend(render_args(std::slice::from_ref(a), &|p| map.render(p)));
                i += 1;
            }
        }
    }
    out
}

enum Line {
    Out(String),
    Err(String),
}

fn spawn_reader<R: AsyncRead + Unpin + Send + 'static>(
    r: R,
    tx: mpsc::Sender<Line>,
    is_err: bool,
) -> tokio::task::JoinHandle<()> {
    // Own task per pipe: `read_until` is not cancel safe inside `select!`.
    tokio::spawn(async move {
        let mut br = BufReader::new(r);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match br.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    while matches!(buf.last(), Some(b'\n' | b'\r')) {
                        buf.pop();
                    }
                    let s = String::from_utf8_lossy(&buf).into_owned();
                    let line = if is_err { Line::Err(s) } else { Line::Out(s) };
                    if tx.send(line).await.is_err() {
                        break;
                    }
                }
            }
        }
    })
}

/// The strings under which this job's sandbox root can appear inside an output:
/// the host spelling, the other separator spelling and the `\\?\` prefixed forms
/// (CF-10). UTF-16 is not searched: rustc writes UTF-8.
fn sandbox_needles(map: &SandboxMap) -> Vec<Vec<u8>> {
    let host = map.root.to_string_lossy().into_owned();
    let host = if cfg!(windows) {
        host.replace('/', "\\")
    } else {
        host
    };
    let other = if host.contains('\\') {
        host.replace('\\', "/")
    } else {
        host.replace('/', "\\")
    };
    let mut v = vec![host.clone(), other.clone()];
    if cfg!(windows) {
        v.push(format!(r"\\?\{host}"));
        v.push(format!("//?/{other}"));
    }
    v.sort();
    v.dedup();
    v.into_iter().map(String::into_bytes).collect()
}

/// Debug-symbol files list the object and output paths of the link by design
/// (a proc-macro `.pdb` always names the sandbox). Nothing loads them at build
/// or run time, so they are the one kind of output the scan skips.
fn scan_exempt(path: &ClientPath) -> bool {
    path.path
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("pdb"))
}

fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    let Some(last_start) = hay.len().checked_sub(needle.len()) else {
        return false;
    };
    let Some(&first) = needle.first() else {
        return false;
    };
    let mut i = 0;
    while i <= last_start {
        match hay[i..=last_start].iter().position(|b| *b == first) {
            Some(off) if hay[i + off..i + off + needle.len()] == *needle => return true,
            Some(off) => i += off + 1,
            None => return false,
        }
    }
    false
}

/// Sends events and remembers what was already emitted as an output.
struct Emitter<'a> {
    ctx: &'a ExecContext<'a>,
    spec: &'a JobSpec,
    map: &'a SandboxMap,
    rewriter: &'a Rewriter,
    events: &'a mpsc::Sender<ExecEvent>,
    /// Outputs already sent (hash of what was sent).
    emitted: HashMap<ClientPath, BlobHash>,
    needles: Vec<Vec<u8>>,
    /// Output blobs stored for the owner so far (dropped again when tainted).
    stored: Vec<BlobHash>,
}

impl Emitter<'_> {
    async fn send(&self, ev: ExecEvent) -> anyhow::Result<()> {
        // The receiver is gone: nobody wants this job any more.
        self.events
            .send(ev)
            .await
            .map_err(|_| anyhow::Error::from(ExecAborted::Cancelled))
    }

    /// Send one produced file (rewriting `.d` files) and store it.
    async fn output(&mut self, path: ClientPath, mut data: Vec<u8>) -> anyhow::Result<()> {
        if path.path.ends_with(".d") {
            data = self
                .rewriter
                .dep_info(&String::from_utf8_lossy(&data))
                .into_bytes();
        }
        // Rewriting removed the sandbox path from `.d` files; anything left in a
        // file is a path rustc did not remap and a local build would not contain.
        if !scan_exempt(&path) && self.needles.iter().any(|n| contains_bytes(&data, n)) {
            let file = path.path.rsplit(['\\', '/']).next().unwrap_or("");
            tracing::info!(
                crate_name = crate::proto::log_safe(&self.spec.crate_name).as_str(),
                file,
                "output embeds the sandbox path"
            );
            for h in self.stored.drain(..) {
                if !self.spec.inputs.iter().any(|i| i.hash == h) {
                    let _ = self.ctx.store.remove(self.ctx.owner, &h);
                }
            }
            self.send(ExecEvent::Tainted(path)).await?;
            return Err(anyhow::Error::from(ExecAborted::Tainted));
        }
        let hash = BlobHash::of(&data);
        if !self.spec.no_retain {
            match self
                .ctx
                .store
                .put(self.ctx.owner, &self.ctx.key, &hash, &data)
            {
                Ok(()) => self.stored.push(hash),
                // The result is still delivered; only the later cache hit is lost.
                Err(e) => tracing::warn!("could not store output {hash:?}: {e:#}"),
            }
        }
        self.emitted.insert(path.clone(), hash);
        self.send(ExecEvent::Output { path, hash, data }).await
    }

    async fn stderr_line(&mut self, line: &str) -> anyhow::Result<()> {
        if let Some((file, _emit)) = rewrite::artifact(line) {
            // The submitter must have the file before cargo sees the notice.
            let sandbox_cwd = sandbox_path(self.map, &self.spec.cwd)?;
            let p = Path::new(&file);
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                sandbox_cwd.join(p)
            };
            let client = self
                .map
                .from_sandbox(&abs, self.spec.cwd.style)
                .context("artifact notification points outside the sandbox")?;
            let data =
                std::fs::read(&abs).with_context(|| format!("read artifact {}", client.path))?;
            self.output(client, data).await?;
        }
        self.send(ExecEvent::Stderr(self.rewriter.stderr_line(line)))
            .await
    }
}

/// Materialize the sandbox (all inputs must be in the store now), run rustc
/// (`toolchain.rustc`) with mapped args + remaps + jobserver, stream events to
/// `events`, collect outputs from the mapped out-dir, delete the sandbox.
/// `cancel` aborts the job (kills only the rustc child it spawned).
///
/// Errors: `ExecAborted::{Timeout, Cancelled}` (downcastable) when stopped from
/// outside; any other error is an infrastructure failure. A rustc that exits
/// with a nonzero code is NOT an error: it yields `Exit(code)`.
pub async fn run(
    ctx: &ExecContext<'_>,
    spec: &JobSpec,
    toolchain: &Installed,
    events: tokio::sync::mpsc::Sender<ExecEvent>,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let job_dir = ctx.jobs_dir.join(job_hex(ctx.owner, &spec.job_id));
    let map = SandboxMap::new(job_dir.join("root"));
    validate_spec(spec, ctx.accept_env, &toolchain.id.host, &ctx.jobs_dir)?;
    validate_paths(spec, &map)?;
    // Claimed before anything is deleted: a duplicate (owner, id) must not wipe
    // the sandbox of the job that is running.
    let _running = RunningGuard::claim(ctx, spec)?;
    // The inputs stay in the store while this job runs, whatever other jobs
    // delete or evict. Dropped after the `no_retain` cleanup below, which then
    // takes effect right away.
    let _pin = ctx.store.pin(ctx.owner, spec.inputs.iter().map(|i| i.hash));
    let _guard = DirGuard(job_dir.clone());
    // A directory left by a crashed run would leak old files into this job.
    let _ = std::fs::remove_dir_all(&job_dir);

    let result = run_in_sandbox(ctx, spec, toolchain, &events, &cancel, &map).await;

    remove_job_dir(&job_dir).await;
    if spec.no_retain {
        for i in &spec.inputs {
            let _ = ctx.store.remove(ctx.owner, &i.hash);
        }
    }
    result
}

async fn run_in_sandbox(
    ctx: &ExecContext<'_>,
    spec: &JobSpec,
    toolchain: &Installed,
    events: &mpsc::Sender<ExecEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    map: &SandboxMap,
) -> anyhow::Result<()> {
    let style = spec.cwd.style;

    // 1. Materialize inputs.
    let mut input_info: HashMap<ClientPath, (BlobHash, u64)> = HashMap::new();
    let mut written: u64 = 0;
    for input in &spec.inputs {
        let dst = sandbox_path(map, &input.path)?;
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let data = ctx
            .store
            .get(ctx.owner, &ctx.key, &input.hash)
            .with_context(|| format!("input {}", input.path.path))?;
        // The declared sizes were checked up front; the real ones are checked
        // again before every write.
        written = written.saturating_add(data.len() as u64);
        if written > guard::MAX_MATERIALIZED_BYTES {
            bail!("job inputs exceed {} bytes", guard::MAX_MATERIALIZED_BYTES);
        }
        std::fs::write(&dst, &data).with_context(|| format!("write input {}", input.path.path))?;
        #[cfg(unix)]
        if input.executable {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;
        }
        input_info.insert(input.path.clone(), (input.hash, data.len() as u64));
        // Large jobs have hundreds of files; let other tasks run in between.
        tokio::task::yield_now().await;
    }
    for d in spec.dirs.iter().chain([&spec.out_dir, &spec.cwd]) {
        let dir = sandbox_path(map, d)?;
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    }

    // 2. Arguments: our remaps first, so the user's own remaps (later) win.
    let remaps = map.remaps(spec_paths(spec));
    let mut args: Vec<String> = Vec::new();
    for (from, to) in &remaps {
        args.push("--remap-path-prefix".into());
        args.push(format!("{from}={to}"));
    }
    args.extend(render_job_args(spec, map, toolchain, &ctx.jobs_dir));

    let rewriter = Rewriter {
        pairs: remaps,
        client_windows: style == crate::proto::PathStyle::Windows,
    };

    // 3. Environment and process.
    let mut cmd = std::process::Command::new(&toolchain.rustc);
    cmd.args(&args);
    cmd.current_dir(sandbox_path(map, &spec.cwd)?);
    cmd.env_clear();
    for (k, v) in std::env::vars_os() {
        if !k.to_str().is_some_and(is_build_var) {
            cmd.env(k, v);
        }
    }
    for (k, v) in &spec.env {
        match v {
            EnvValue::Plain(s) => cmd.env(k, s),
            EnvValue::Path(p) => cmd.env(k, map.render(p)),
        };
    }
    // After the environment: configure() sets the MAKEFLAGS family.
    ctx.jobserver.client.configure(&mut cmd);
    crate::proctree::configure(&mut cmd);
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().with_context(|| {
        format!(
            "spawn rustc for {}",
            crate::proto::log_safe(&spec.crate_name)
        )
    })?;
    // Everything rustc starts (linker, cc) is killed with it.
    let mut tree = match crate::proctree::ProcessTree::adopt(&child) {
        Ok(t) => t,
        Err(e) => {
            let _ = child.kill().await;
            return Err(e.context("track the rustc process tree"));
        }
    };
    let stdout = child.stdout.take().context("no stdout pipe")?;
    let stderr = child.stderr.take().context("no stderr pipe")?;
    let (tx, mut rx) = mpsc::channel::<Line>(64);
    let readers = [
        spawn_reader(stdout, tx.clone(), false),
        spawn_reader(stderr, tx, true),
    ];

    let deadline = tokio::time::Instant::now() + ctx.timeout;
    let mut em = Emitter {
        ctx,
        spec,
        map,
        rewriter: &rewriter,
        events,
        emitted: HashMap::new(),
        needles: sandbox_needles(map),
        stored: Vec::new(),
    };

    let outcome = async {
        em.send(ExecEvent::Started).await?;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(anyhow::Error::from(ExecAborted::Cancelled)),
                _ = tokio::time::sleep_until(deadline) => return Err(anyhow::Error::from(ExecAborted::Timeout)),
                line = rx.recv() => match line {
                    Some(Line::Err(l)) => em.stderr_line(&l).await?,
                    Some(Line::Out(l)) => em.send(ExecEvent::Stdout(rewriter.text(&l))).await?,
                    None => break,
                },
            }
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(anyhow::Error::from(ExecAborted::Cancelled)),
            _ = tokio::time::sleep_until(deadline) => Err(anyhow::Error::from(ExecAborted::Timeout)),
            st = child.wait() => Ok(st.context("wait for rustc")?),
        }
    }
    .await;

    let status = match outcome {
        Ok(s) => s,
        Err(e) => {
            // Kill only the tree of the child this call spawned.
            tree.kill();
            let _ = child.kill().await;
            for r in &readers {
                r.abort();
            }
            return Err(e);
        }
    };
    tree.finished();
    let code = status.code().unwrap_or(-1);

    // 4. Outputs. After a failed compile the out-dir only holds partial files
    // that cargo would ignore, so nothing is collected.
    if code == 0 {
        let out_root = sandbox_path(map, &spec.out_dir)?;
        let mut files: Vec<PathBuf> = walkdir::WalkDir::new(&out_root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .collect();
        files.sort();
        for f in files {
            let Some(client) = map.from_sandbox(&f, style) else {
                continue;
            };
            let size = std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
            let data = std::fs::read(&f).with_context(|| format!("read output {}", client.path))?;
            // Unchanged inputs living in the out-dir (dependency rlibs in
            // `deps/`) are not outputs.
            if let Some((h, sz)) = input_info.get(&client)
                && *sz == size
                && BlobHash::of(&data) == *h
            {
                continue;
            }
            // `.d` content changes through rewriting, so compare what would be sent.
            let sent = if client.path.ends_with(".d") {
                rewriter
                    .dep_info(&String::from_utf8_lossy(&data))
                    .into_bytes()
            } else {
                data.clone()
            };
            if em.emitted.get(&client) == Some(&BlobHash::of(&sent)) {
                continue;
            }
            em.output(client, data).await?;
        }
    }
    em.send(ExecEvent::Exit(code)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{InputFile, PathStyle};
    use crate::toolchain::{self, Toolchains};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    const KEY: [u8; 32] = [42; 32];
    const OWNER: NodeId = NodeId([9; 16]);

    fn cp(p: &Path) -> ClientPath {
        ClientPath::from_local(p, Path::new(".")).expect("absolute test path")
    }

    fn active_toolchain(data: &Path) -> Installed {
        let id = toolchain::query(Path::new("rustc")).unwrap();
        let tc = Toolchains::scan(data, false).unwrap();
        tc.list()
            .into_iter()
            .find(|t| t.id.same_compiler(&id))
            .expect("active toolchain")
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        jobs: PathBuf,
        store: BlobStore,
        js: NodeJobserver,
        tc: Installed,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        // Everything under one temp dir; the "client" tree is `proj`.
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let store = BlobStore::open(&tmp.path().join("cache"), u64::MAX).unwrap();
        let tc = active_toolchain(tmp.path());
        let jobs = tmp.path().join("jobs");
        Fixture {
            root,
            jobs,
            store,
            js: NodeJobserver::new(2).unwrap(),
            tc,
            _tmp: tmp,
        }
    }

    /// A one-file lib crate job with a warning in its source.
    fn lib_spec(f: &Fixture, src: &str, no_retain: bool) -> JobSpec {
        let lib = f.root.join("src").join("lib.rs");
        let out = f.root.join("out");
        let data = src.as_bytes();
        let hash = BlobHash::of(data);
        f.store.put(OWNER, &KEY, &hash, data).unwrap();
        let arg = |s: &str| Arg::Plain(s.to_string());
        JobSpec {
            job_id: JobId([1; 16]),
            toolchain: f.tc.id.clone(),
            target: f.tc.id.host.clone(),
            args: vec![
                arg("--crate-name"),
                arg("fixture"),
                arg("--edition=2021"),
                Arg::Path(cp(&lib)),
                arg("--crate-type"),
                arg("lib"),
                arg("--emit=dep-info,metadata,link"),
                arg("--error-format=json"),
                arg("--json=artifacts,diagnostic-rendered-ansi"),
                arg("--out-dir"),
                Arg::Path(cp(&out)),
                arg("--target"),
                arg(&f.tc.id.host),
                arg("-C"),
                arg("extra-filename=-abc"),
            ],
            cwd: cp(&f.root),
            env: vec![("CARGO_MANIFEST_DIR".into(), EnvValue::Path(cp(&f.root)))],
            inputs: vec![InputFile {
                path: cp(&lib),
                hash,
                size: data.len() as u64,
                executable: false,
            }],
            extra_env: Vec::new(),
            dirs: vec![cp(&out)],
            out_dir: cp(&out),
            crate_name: "fixture".into(),
            no_retain,
            needs_host: false,
        }
    }

    fn ctx<'a>(f: &'a Fixture, timeout: Duration) -> ExecContext<'a> {
        ExecContext {
            jobs_dir: f.jobs.clone(),
            store: &f.store,
            jobserver: &f.js,
            owner: OWNER,
            key: KEY,
            timeout,
            accept_env: &[],
        }
    }

    async fn collect(
        c: &ExecContext<'_>,
        spec: &JobSpec,
        tc: &Installed,
        cancel: CancellationToken,
    ) -> (anyhow::Result<()>, Vec<ExecEvent>) {
        let (tx, mut rx) = mpsc::channel(1024);
        let res = run(c, spec, tc, tx, cancel).await;
        let mut evs = Vec::new();
        while let Some(e) = rx.recv().await {
            evs.push(e);
        }
        (res, evs)
    }

    const SRC: &str = "pub fn answer() -> u32 {\n    let unused = 1;\n    42\n}\n";

    #[tokio::test]
    async fn builds_lib_end_to_end() {
        let f = fixture();
        let spec = lib_spec(&f, SRC, false);
        let c = ctx(&f, Duration::from_secs(120));
        let (res, evs) = collect(&c, &spec, &f.tc, CancellationToken::new()).await;
        res.unwrap();

        assert!(matches!(evs.first(), Some(ExecEvent::Started)));
        assert!(
            matches!(evs.last(), Some(ExecEvent::Exit(0))),
            "events: {evs:?}"
        );
        let names: Vec<String> = evs
            .iter()
            .filter_map(|e| match e {
                ExecEvent::Output { path, .. } => {
                    Some(path.path.rsplit(['\\', '/']).next().unwrap().to_string())
                }
                _ => None,
            })
            .collect();
        assert!(
            names.contains(&"libfixture-abc.rmeta".to_string()),
            "{names:?}"
        );
        assert!(
            names.contains(&"libfixture-abc.rlib".to_string()),
            "{names:?}"
        );
        assert!(names.contains(&"fixture-abc.d".to_string()), "{names:?}");
        // Each file exactly once: a file already sent with its final content is not repeated.
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate outputs: {names:?}");

        // The rmeta Output precedes the stderr line announcing it.
        let rmeta_out = evs
            .iter()
            .position(
                |e| matches!(e, ExecEvent::Output { path, .. } if path.path.ends_with(".rmeta")),
            )
            .unwrap();
        let rmeta_note = evs
            .iter()
            .position(|e| matches!(e, ExecEvent::Stderr(l) if l.contains("\"artifact\"") && l.contains("rmeta")))
            .expect("rmeta artifact line");
        assert!(
            rmeta_out < rmeta_note,
            "Output must come before its notification"
        );

        // Paths: client paths only.
        let sandbox_root = f.jobs.to_string_lossy().replace('/', "\\");
        let client_root = cp(&f.root).path;
        let mut saw_warning = false;
        for e in &evs {
            match e {
                ExecEvent::Stderr(l) => {
                    assert!(!l.contains(&sandbox_root), "sandbox path leaked: {l}");
                    if l.contains("unused variable") {
                        saw_warning = true;
                        // JSON-escaped backslashes are fine: compare on the unescaped text.
                        let v: serde_json::Value = serde_json::from_str(l).unwrap();
                        let rendered = v["rendered"].as_str().unwrap();
                        assert!(rendered.contains("lib.rs"), "{rendered}");
                        assert!(
                            v["spans"][0]["file_name"]
                                .as_str()
                                .unwrap()
                                .starts_with(&client_root),
                            "{l}"
                        );
                    }
                }
                ExecEvent::Output { path, data, hash } if path.path.ends_with(".d") => {
                    let text = String::from_utf8(data.clone()).unwrap();
                    assert!(
                        !text.contains(&sandbox_root),
                        "dep-info leaked the sandbox: {text}"
                    );
                    assert!(text.contains("lib.rs"));
                    assert_eq!(*hash, BlobHash::of(data));
                }
                _ => {}
            }
        }
        assert!(saw_warning, "no warning diagnostic in {evs:?}");

        // Outputs were stored for the owner; the sandbox is gone.
        for e in &evs {
            if let ExecEvent::Output { hash, data, .. } = e {
                assert_eq!(f.store.get(OWNER, &KEY, hash).unwrap(), *data);
            }
        }
        assert!(
            !f.jobs.join(job_hex(OWNER, &spec.job_id)).exists(),
            "sandbox not deleted"
        );
    }

    #[tokio::test]
    async fn outputs_are_collected_without_artifact_notifications() {
        // Human error format, no --json=artifacts: only the exit-time scan finds the files.
        let f = fixture();
        let mut spec = lib_spec(&f, SRC, false);
        spec.args.retain(|a| !matches!(a, Arg::Plain(s) if s.starts_with("--json=") || s.starts_with("--error-format=")));
        let c = ctx(&f, Duration::from_secs(120));
        let (res, evs) = collect(&c, &spec, &f.tc, CancellationToken::new()).await;
        res.unwrap();
        let mut names: Vec<String> = evs
            .iter()
            .filter_map(|e| match e {
                ExecEvent::Output { path, .. } => {
                    Some(path.path.rsplit(['\\', '/']).next().unwrap().to_string())
                }
                _ => None,
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "fixture-abc.d",
                "libfixture-abc.rlib",
                "libfixture-abc.rmeta"
            ]
        );
        let n = evs.len();
        assert!(matches!(evs[n - 1], ExecEvent::Exit(0)));
        assert!(
            matches!(evs[n - 2], ExecEvent::Output { .. }),
            "outputs come right before Exit"
        );
        // Human diagnostics carry the client path too.
        let client_root = cp(&f.root).path;
        assert!(
            evs.iter()
                .any(|e| matches!(e, ExecEvent::Stderr(l) if l.contains(&client_root))),
            "{evs:?}"
        );
        let sandbox_root = f.jobs.to_string_lossy().replace('/', "\\");
        assert!(
            !evs.iter()
                .any(|e| matches!(e, ExecEvent::Stderr(l) if l.contains(&sandbox_root)))
        );
    }

    #[tokio::test]
    async fn no_retain_deletes_input_blobs_and_stores_no_outputs() {
        let f = fixture();
        let spec = lib_spec(&f, SRC, true);
        let c = ctx(&f, Duration::from_secs(120));
        let (res, evs) = collect(&c, &spec, &f.tc, CancellationToken::new()).await;
        res.unwrap();
        assert!(matches!(evs.last(), Some(ExecEvent::Exit(0))));
        assert!(!f.store.has(OWNER, &spec.inputs[0].hash), "input blob kept");
        for e in &evs {
            if let ExecEvent::Output { hash, .. } = e {
                assert!(!f.store.has(OWNER, hash), "output stored despite no_retain");
            }
        }
    }

    #[tokio::test]
    async fn compile_error_yields_exit_code_and_cleans_up() {
        let f = fixture();
        let spec = lib_spec(&f, "pub fn broken( {\n", false);
        let c = ctx(&f, Duration::from_secs(120));
        let (res, evs) = collect(&c, &spec, &f.tc, CancellationToken::new()).await;
        res.unwrap();
        assert!(
            matches!(evs.last(), Some(ExecEvent::Exit(code)) if *code != 0),
            "{evs:?}"
        );
        assert!(
            evs.iter()
                .any(|e| matches!(e, ExecEvent::Stderr(l) if l.contains("error")))
        );
        assert!(
            !evs.iter().any(|e| matches!(e, ExecEvent::Output { .. })),
            "outputs after a failed compile"
        );
        assert!(!f.jobs.join(job_hex(OWNER, &spec.job_id)).exists());
    }

    /// A stand-in "rustc" that just sleeps: a tiny program compiled with the
    /// real rustc, so the kill path is exercised without any CPU load.
    fn fake_sleeper(f: &Fixture) -> Installed {
        let dir = f._tmp.path().join("fake");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("sleeper.rs");
        std::fs::write(
            &src,
            "fn main() { std::thread::sleep(std::time::Duration::from_secs(120)); }",
        )
        .unwrap();
        let exe = dir.join(if cfg!(windows) {
            "sleeper.exe"
        } else {
            "sleeper"
        });
        let st = std::process::Command::new(&f.tc.rustc)
            .arg(&src)
            .arg("-o")
            .arg(&exe)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success());
        Installed {
            rustc: exe,
            ..f.tc.clone()
        }
    }

    /// A stand-in "rustc" that starts a sleeping grandchild (like rustc starting
    /// a linker) and records its pid in the file named by CRABFARM_TEST_PIDFILE.
    fn fake_with_grandchild(f: &Fixture) -> Installed {
        let dir = f._tmp.path().join("fakegc");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("parent.rs");
        std::fs::write(
            &src,
            r#"
use std::process::{Command, Stdio};
fn main() {
    if std::env::args().any(|a| a == "--grandchild") {
        std::thread::sleep(std::time::Duration::from_secs(120));
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let child = Command::new(exe).arg("--grandchild").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    std::fs::write(std::env::var("CRABFARM_TEST_PIDFILE").unwrap(), child.id().to_string()).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(120));
}
"#,
        )
        .unwrap();
        let exe = dir.join(if cfg!(windows) {
            "parent.exe"
        } else {
            "parent"
        });
        let st = std::process::Command::new(&f.tc.rustc)
            .arg(&src)
            .arg("-o")
            .arg(&exe)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success());
        Installed {
            rustc: exe,
            ..f.tc.clone()
        }
    }

    #[cfg(windows)]
    fn pid_alive(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: plain Win32 queries on a handle opened and closed here.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code);
            CloseHandle(h);
            ok != 0 && code == 259 // STILL_ACTIVE
        }
    }

    #[cfg(unix)]
    fn pid_alive(pid: u32) -> bool {
        // SAFETY: signal 0 only checks existence.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    /// CF-9: on timeout the whole process tree ends, not only rustc, and the
    /// sandbox is removed.
    /// CF-10: a crate that embeds its manifest dir (`env!`) carries the sandbox
    /// path; the job reports it instead of sending any output.
    #[tokio::test]
    async fn output_with_the_sandbox_path_is_reported_not_sent() {
        let f = fixture();
        let src = "pub const DIR: &str = env!(\"CARGO_MANIFEST_DIR\");\n";
        let spec = lib_spec(&f, src, false);
        let c = ctx(&f, Duration::from_secs(120));
        let (res, evs) = collect(&c, &spec, &f.tc, CancellationToken::new()).await;
        let err = res.unwrap_err();
        assert_eq!(
            err.downcast_ref::<ExecAborted>(),
            Some(&ExecAborted::Tainted),
            "{err:#}"
        );
        assert!(
            evs.iter().any(|e| matches!(e, ExecEvent::Tainted(_))),
            "{evs:?}"
        );
        assert!(
            !evs.iter().any(|e| matches!(e, ExecEvent::Exit(_))),
            "{evs:?}"
        );
        // Nothing after the report, and nothing of this job stays in the store.
        let n = evs
            .iter()
            .position(|e| matches!(e, ExecEvent::Tainted(_)))
            .unwrap();
        assert!(
            !evs[n + 1..]
                .iter()
                .any(|e| matches!(e, ExecEvent::Output { .. })),
            "{evs:?}"
        );
        for e in &evs {
            if let ExecEvent::Output { hash, .. } = e {
                assert!(!f.store.has(OWNER, hash), "tainted job left an output blob");
            }
        }
        assert!(!f.jobs.join(job_hex(OWNER, &spec.job_id)).exists());
    }

    #[test]
    fn only_debug_symbol_files_skip_the_scan() {
        let p = |s: &str| ClientPath {
            style: PathStyle::Windows,
            path: s.into(),
        };
        assert!(scan_exempt(&p(r"C:\t\derive_mac-1.PDB")));
        for s in [
            r"C:\t\libx-1.rlib",
            r"C:\t\libx-1.rmeta",
            r"C:\t\x-1.d",
            r"C:\t\x.dll",
            r"C:\t\pdb",
            r"C:\t\x.pdb.rlib",
        ] {
            assert!(!scan_exempt(&p(s)), "{s}");
        }
    }

    #[test]
    fn needles_cover_both_separators_and_the_prefixed_form() {
        let map = SandboxMap::new(PathBuf::from("E:/data/jobs/aa-bb/root"));
        let n: Vec<String> = sandbox_needles(&map)
            .into_iter()
            .map(|b| String::from_utf8(b).unwrap())
            .collect();
        if cfg!(windows) {
            assert!(n.contains(&r"E:\data\jobs\aa-bb\root".to_string()), "{n:?}");
            assert!(n.contains(&"E:/data/jobs/aa-bb/root".to_string()), "{n:?}");
            assert!(
                n.contains(&r"\\?\E:\data\jobs\aa-bb\root".to_string()),
                "{n:?}"
            );
        } else {
            assert!(n.contains(&"E:/data/jobs/aa-bb/root".to_string()), "{n:?}");
        }
        assert!(contains_bytes(
            b"xx\0E:/data/jobs/aa-bb/root/C/a\0",
            b"E:/data/jobs/aa-bb/root"
        ));
        assert!(!contains_bytes(
            b"E:/data/jobs/aa-bb/roo",
            b"E:/data/jobs/aa-bb/root"
        ));
        assert!(!contains_bytes(b"", b"x"));
        assert!(
            contains_bytes(b"EEE:/x", b"E:/x"),
            "a false start must not hide the match"
        );
    }

    /// CF-10 (from CF-14): the inputs of a running job survive the `no_retain`
    /// cleanup of another job that shares the blob.
    #[tokio::test]
    async fn running_job_keeps_its_inputs_when_another_job_cleans_up() {
        let f = fixture();
        let fake = fake_sleeper(&f);
        let spec = lib_spec(&f, SRC, false);
        let hash = spec.inputs[0].hash;
        let cancel = CancellationToken::new();
        let c = ctx(&f, Duration::from_secs(120));
        let running = {
            let (spec, fake, cancel) = (spec.clone(), fake.clone(), cancel.clone());
            let c = ctx(&f, Duration::from_secs(120));
            async move {
                // The receiver must live as long as the job: a closed channel cancels it.
                let (tx, _rx) = mpsc::channel(1024);
                run(&c, &spec, &fake, tx, cancel).await
            }
        };
        let other = async {
            let dir = f.jobs.join(job_hex(OWNER, &spec.job_id));
            for _ in 0..100 {
                if dir.join("root").exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            // A second job (another id, `no_retain`) shares the blob and finishes.
            let mut second = lib_spec(&f, SRC, true);
            second.job_id = JobId([2; 16]);
            let (res, _) = collect(&c, &second, &f.tc, CancellationToken::new()).await;
            res.unwrap();
            assert!(
                f.store.has(OWNER, &hash),
                "the running job's input was deleted"
            );
            cancel.cancel();
        };
        let (res, ()) = tokio::join!(running, other);
        assert!(res.is_err());
        // The cleanup the second job asked for happens once nobody needs the blob.
        assert!(
            !f.store.has(OWNER, &hash),
            "deferred removal never happened"
        );
    }

    #[tokio::test]
    async fn timeout_kills_grandchildren_and_removes_the_sandbox() {
        let f = fixture();
        let fake = fake_with_grandchild(&f);
        let pidfile = f._tmp.path().join("grandchild.pid");
        let mut spec = lib_spec(&f, SRC, false);
        spec.extra_env.push("CRABFARM_TEST_PIDFILE".into());
        spec.env.push((
            "CRABFARM_TEST_PIDFILE".into(),
            EnvValue::Plain(pidfile.to_string_lossy().into_owned()),
        ));
        let accept = vec!["CRABFARM_TEST_PIDFILE".to_string()];
        let mut c = ctx(&f, Duration::from_millis(3000));
        c.accept_env = &accept;
        let (res, _evs) = collect(&c, &spec, &fake, CancellationToken::new()).await;
        let err = res.unwrap_err();
        assert_eq!(
            err.downcast_ref::<ExecAborted>(),
            Some(&ExecAborted::Timeout),
            "{err:#}"
        );
        let pid: u32 = std::fs::read_to_string(&pidfile)
            .expect("the fake rustc started its grandchild")
            .trim()
            .parse()
            .unwrap();
        let mut alive = true;
        for _ in 0..50 {
            alive = pid_alive(pid);
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if alive {
            // Clean up our own recorded grandchild before failing the test.
            #[cfg(windows)]
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/PID", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
        assert!(!alive, "the grandchild survived the timeout");
        assert!(
            !f.jobs.join(job_hex(OWNER, &spec.job_id)).exists(),
            "sandbox not deleted"
        );
    }

    /// CF-9: a second request with the same (owner, job id) is refused and must
    /// not wipe the running job's sandbox; another owner with the same id is fine.
    #[tokio::test]
    async fn duplicate_job_is_refused_without_touching_the_running_one() {
        let f = fixture();
        let fake = fake_sleeper(&f);
        let spec = lib_spec(&f, SRC, false);
        let dir = f.jobs.join(job_hex(OWNER, &spec.job_id));
        assert_ne!(
            job_hex(OWNER, &spec.job_id),
            job_hex(NodeId([8; 16]), &spec.job_id)
        );

        let cancel = CancellationToken::new();
        let first = {
            let c = ctx(&f, Duration::from_secs(120));
            let (spec, fake, cancel) = (spec.clone(), fake.clone(), cancel.clone());
            async move {
                // The receiver must live as long as the job: a closed channel cancels it.
                let (tx, _rx) = mpsc::channel(1024);
                run(&c, &spec, &fake, tx, cancel).await
            }
        };
        let canceller = async {
            // Wait until the first job has its sandbox, then try the duplicate.
            for _ in 0..100 {
                if dir.join("root").exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(dir.join("root").exists(), "first job never started");
            let marker = dir.join("root").join("marker");
            std::fs::write(&marker, b"alive").unwrap();
            let c = ctx(&f, Duration::from_secs(120));
            let (res, evs) = collect(&c, &spec, &fake, CancellationToken::new()).await;
            let msg = format!("{:#}", res.expect_err("duplicate must be refused"));
            assert!(msg.contains("already running"), "{msg}");
            assert!(evs.is_empty());
            assert!(
                marker.exists(),
                "the duplicate wiped the running job's sandbox"
            );
            cancel.cancel();
        };
        let (res, ()) = tokio::join!(first, canceller);
        assert_eq!(
            res.unwrap_err().downcast_ref::<ExecAborted>(),
            Some(&ExecAborted::Cancelled)
        );
        assert!(!dir.exists());
    }

    /// CF-9: a directory that cannot be removed at once is retried until it can.
    #[cfg(windows)]
    #[test]
    fn locked_directory_is_removed_once_the_lock_goes() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("job");
        std::fs::create_dir_all(&dir).unwrap();
        // share_mode(0): nobody may delete or open the file while this handle lives.
        let held = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(dir.join("locked.rs"))
            .unwrap();
        assert!(
            std::fs::remove_dir_all(&dir).is_err(),
            "the lock must block the removal"
        );
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            drop(held);
        });
        assert!(retry_remove(
            &dir,
            20,
            Duration::from_millis(100),
            Duration::from_millis(200)
        ));
        assert!(!dir.exists());
        releaser.join().unwrap();
        // And it gives up (returns false) while the lock stays.
        let dir2 = d.path().join("job2");
        std::fs::create_dir_all(&dir2).unwrap();
        let _held = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(dir2.join("locked.rs"))
            .unwrap();
        assert!(!retry_remove(
            &dir2,
            3,
            Duration::from_millis(10),
            Duration::from_millis(10)
        ));
        assert!(dir2.exists());
    }

    #[test]
    fn sweep_removes_leftover_sandboxes() {
        let d = tempfile::tempdir().unwrap();
        let jobs = d.path().join("jobs");
        std::fs::create_dir_all(jobs.join("aa-bb").join("root").join("C")).unwrap();
        std::fs::write(
            jobs.join("aa-bb").join("root").join("C").join("secret.rs"),
            b"plaintext",
        )
        .unwrap();
        std::fs::write(jobs.join("stray-file"), b"x").unwrap();
        sweep_jobs(&jobs);
        assert_eq!(std::fs::read_dir(&jobs).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn timeout_kills_child_and_cleans_up() {
        let f = fixture();
        let fake = fake_sleeper(&f);
        let spec = lib_spec(&f, SRC, false);
        let c = ctx(&f, Duration::from_millis(600));
        let started = std::time::Instant::now();
        let (res, evs) = collect(&c, &spec, &fake, CancellationToken::new()).await;
        let err = res.unwrap_err();
        assert_eq!(
            err.downcast_ref::<ExecAborted>(),
            Some(&ExecAborted::Timeout),
            "{err:#}"
        );
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(matches!(evs.first(), Some(ExecEvent::Started)));
        assert!(!evs.iter().any(|e| matches!(e, ExecEvent::Exit(_))));
        assert!(
            !f.jobs.join(job_hex(OWNER, &spec.job_id)).exists(),
            "sandbox not deleted after timeout"
        );
    }

    #[tokio::test]
    async fn cancel_kills_child_and_cleans_up() {
        let f = fixture();
        let fake = fake_sleeper(&f);
        let spec = lib_spec(&f, SRC, false);
        let c = ctx(&f, Duration::from_secs(120));
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            c2.cancel();
        });
        let (res, _evs) = collect(&c, &spec, &fake, cancel).await;
        let err = res.unwrap_err();
        assert_eq!(
            err.downcast_ref::<ExecAborted>(),
            Some(&ExecAborted::Cancelled)
        );
        assert!(!f.jobs.join(job_hex(OWNER, &spec.job_id)).exists());
    }

    #[test]
    fn missing_inputs_lists_absent_blobs_once() {
        let f = fixture();
        let mut spec = lib_spec(&f, SRC, false);
        let ghost = BlobHash::of(b"never stored");
        for n in ["a.rs", "b.rs"] {
            spec.inputs.push(InputFile {
                path: cp(&f.root.join(n)),
                hash: ghost,
                size: 12,
                executable: false,
            });
        }
        let c = ctx(&f, Duration::from_secs(1));
        assert_eq!(missing_inputs(&c, &spec), vec![ghost]);
    }

    #[test]
    fn build_vars_are_filtered_system_vars_kept() {
        for n in [
            "CARGO_HOME",
            "cargo_pkg_name",
            "RUSTFLAGS",
            "RUSTC_WRAPPER",
            "RUSTDOCFLAGS",
            "OUT_DIR",
            "DEP_X_Y",
            "TARGET",
            "HOST",
            "PROFILE",
            "NUM_JOBS",
            "OPT_LEVEL",
            "DEBUG",
            "MAKEFLAGS",
            "MFLAGS",
            "CARGO_MAKEFLAGS",
        ] {
            assert!(is_build_var(n), "{n}");
        }
        for n in [
            "PATH",
            "SystemRoot",
            "LIB",
            "INCLUDE",
            "RUSTUP_HOME",
            "TEMP",
            "HOME",
        ] {
            assert!(!is_build_var(n), "{n}");
        }
    }

    /// CF-8: a hostile spec is refused before rustc starts or anything is written.
    #[tokio::test]
    async fn hostile_specs_are_refused_before_anything_runs() {
        let f = fixture();
        let escape = f._tmp.path().join("escaped");
        let escape_arg = escape.to_string_lossy().into_owned();
        let c = ctx(&f, Duration::from_secs(120));
        type Mutate = Box<dyn Fn(&mut JobSpec)>;
        let hostile: Vec<Mutate> = vec![
            Box::new({
                let e = escape_arg.clone();
                move |s| s.args.push(Arg::Plain(format!("--out-dir={e}")))
            }),
            Box::new({
                let e = escape_arg.clone();
                move |s| {
                    s.args
                        .extend([Arg::Plain("-o".into()), Arg::Plain(e.clone())])
                }
            }),
            Box::new({
                let e = escape_arg.clone();
                move |s| s.args.push(Arg::Plain(format!("--emit=link={e}")))
            }),
            Box::new(|s| {
                s.args.extend([
                    Arg::Plain("-C".into()),
                    Arg::Plain("linker=evil-linker".into()),
                    Arg::Plain("-C".into()),
                    Arg::Plain("incremental=x".into()),
                ])
            }),
            Box::new(|s| s.args.push(Arg::Plain("@argfile.txt".into()))),
            Box::new(|s| s.args.push(Arg::Plain("-Zunstable-options".into()))),
            Box::new(|s| {
                s.env
                    .push(("PATH".into(), EnvValue::Plain("C:\\evil".into())))
            }),
            Box::new(|s| {
                s.env
                    .push(("RUSTC_WRAPPER".into(), EnvValue::Plain("evil".into())))
            }),
            Box::new(|s| {
                s.env
                    .push(("LD_PRELOAD".into(), EnvValue::Plain("evil.so".into())))
            }),
            Box::new(|s| {
                s.env
                    .push(("UNDECLARED_NAME".into(), EnvValue::Plain("x".into())))
            }),
        ];
        for (n, mutate) in hostile.iter().enumerate() {
            let mut spec = lib_spec(&f, SRC, false);
            mutate(&mut spec);
            let (res, evs) = collect(&c, &spec, &f.tc, CancellationToken::new()).await;
            let err = res.expect_err(&format!("case {n} must be refused"));
            assert!(
                format!("{err:#}").contains("job refused"),
                "case {n}: {err:#}"
            );
            assert!(evs.is_empty(), "case {n}: events {evs:?}");
            assert!(
                !escape.exists(),
                "case {n}: rustc wrote outside the sandbox"
            );
        }
    }

    /// CF-19: two inputs whose names are one file on this sandbox file system
    /// must not both be written; the job is refused before anything is.
    #[tokio::test]
    async fn colliding_inputs_are_refused_on_a_case_insensitive_sandbox() {
        let f = fixture();
        if !sandbox_case_insensitive(&f.jobs) {
            return; // a case-sensitive file system keeps them apart
        }
        let c = ctx(&f, Duration::from_secs(120));
        let mut spec = lib_spec(&f, SRC, false);
        let mut twin = spec.inputs[0].clone();
        twin.path = ClientPath::parse(twin.path.style, &twin.path.path.to_uppercase())
            .expect("upper-case twin");
        twin.hash = BlobHash::of(b"different content");
        assert_ne!(twin.path, spec.inputs[0].path);
        spec.inputs.push(twin);
        let (res, evs) = collect(&c, &spec, &f.tc, CancellationToken::new()).await;
        let err = res.expect_err("a collision must be refused");
        assert!(format!("{err:#}").contains("job refused"), "{err:#}");
        assert!(evs.is_empty(), "{evs:?}");
    }

    #[test]
    fn linker_path_is_dropped_unless_this_peer_has_it() {
        let f = fixture();
        let map = SandboxMap::new(f.jobs.join("x").join("root"));
        let exe_dir = f._tmp.path().join("tools");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let link = exe_dir.join(if cfg!(windows) { "link.exe" } else { "link" });
        std::fs::write(&link, b"x").unwrap();
        let other = exe_dir.join(if cfg!(windows) { "evil.exe" } else { "evil" });
        std::fs::write(&other, b"x").unwrap();

        let render = |path: &Path, host: &str| {
            let mut spec = lib_spec(&f, SRC, false);
            spec.toolchain.host = host.to_string();
            spec.args.push(Arg::Plain("-C".into()));
            spec.args.push(Arg::PrefixedPath {
                prefix: "linker=".into(),
                path: cp(path),
            });
            spec.args.push(Arg::PrefixedPath {
                prefix: "-Clinker=".into(),
                path: cp(path),
            });
            render_job_args(&spec, &map, &f.tc, &f.jobs)
        };
        let same = f.tc.id.host.clone();
        let count = |args: &[String]| args.iter().filter(|a| a.contains("linker=")).count();
        // Same host, known linker that exists: kept as the native path.
        let kept = render(&link, &same);
        assert_eq!(count(&kept), 2, "{kept:?}");
        assert!(
            kept.iter()
                .any(|a| a == &format!("-Clinker={}", cp(&link).path))
        );
        // The argument after a dropped `-C` must not be left dangling.
        // Different host, missing file, unknown program, file inside the job sandboxes: dropped.
        assert_eq!(count(&render(&link, "other-host-triple")), 0);
        assert_eq!(count(&render(&exe_dir.join("missing.exe"), &same)), 0);
        assert_eq!(count(&render(&other, &same)), 0);
        let inside = f.jobs.join("x").join("root").join("cc");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        std::fs::write(&inside, b"x").unwrap();
        assert_eq!(count(&render(&inside, &same)), 0);
        let dropped = render(&other, &same);
        for (i, a) in dropped.iter().enumerate() {
            if a == "-C" {
                assert!(
                    dropped.get(i + 1).is_some_and(|n| !n.starts_with('-')),
                    "dangling -C: {dropped:?}"
                );
            }
        }
    }

    #[test]
    fn rejects_escaping_paths() {
        let f = fixture();
        let mut spec = lib_spec(&f, SRC, false);
        spec.inputs[0].path = ClientPath {
            style: PathStyle::Posix,
            path: "/a/../../etc/x".into(),
        };
        let map = SandboxMap::new(f.jobs.join("x").join("root"));
        assert!(validate_paths(&spec, &map).is_err());
    }

    #[tokio::test]
    async fn job_env_excludes_node_build_vars() {
        // Set in this process: if it leaked into rustc, `env!` in the source would see it.
        // SAFETY-free approach: use a variable name unique to this test and check via option_env!.
        let f = fixture();
        unsafe { std::env::set_var("CARGO_CRABFARM_LEAK_PROBE", "leaked") };
        let src = "pub const P: Option<&str> = option_env!(\"CARGO_CRABFARM_LEAK_PROBE\");\npub const Q: Option<&str> = option_env!(\"CARGO_PKG_DESCRIPTION\");\n";
        let mut spec = lib_spec(&f, src, false);
        spec.env.push((
            "CARGO_PKG_DESCRIPTION".into(),
            EnvValue::Plain("yes".into()),
        ));
        let c = ctx(&f, Duration::from_secs(120));
        let (res, evs) = collect(&c, &spec, &f.tc, CancellationToken::new()).await;
        res.unwrap();
        // option_env! reads are recorded as env-dep lines in the .d file.
        let dep = evs
            .iter()
            .find_map(|e| match e {
                ExecEvent::Output { path, data, .. } if path.path.ends_with(".d") => {
                    Some(String::from_utf8(data.clone()).unwrap())
                }
                _ => None,
            })
            .unwrap();
        assert!(
            dep.contains("# env-dep:CARGO_CRABFARM_LEAK_PROBE\n")
                || dep.contains("# env-dep:CARGO_CRABFARM_LEAK_PROBE\r\n"),
            "{dep}"
        );
        assert!(
            !dep.contains("CARGO_CRABFARM_LEAK_PROBE=leaked"),
            "node variable leaked: {dep}"
        );
        assert!(dep.contains("CARGO_PKG_DESCRIPTION=yes"), "{dep}");
        unsafe { std::env::remove_var("CARGO_CRABFARM_LEAK_PROBE") };
    }
}
