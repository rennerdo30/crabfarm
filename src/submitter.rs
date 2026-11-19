//! Submitting side (DESIGN sections 4, 8, 10): serve the wrappers that talk to
//! this node over IPC. For each `Submit` the node decides where the job runs,
//! builds its inputs, runs it on a peer and applies the result, or hands the
//! wrapper a local slot.

use crate::cas::{self, Assembler};
use crate::ccache::{self, CacheKey, Captured, EntryOutput};
use crate::depgraph::is_proc_macro_lib;
use crate::inputs::{self, CollectError};
use crate::link::Link;
use crate::node::{DIALBACK_WAIT, Node, PendingDial, Reach, lock};
use crate::proto::{
    BlobHash, ClientPath, EnvValue, InputFile, JobId, JobSpec, LocalReason, Message, NodeId,
};
use crate::scheduler::{self, Candidate, Choice, JobNeeds};
use anyhow::bail;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// After a remote `Exit` the wrapper may still ask for a local retry.
const RETRY_WAIT: Duration = Duration::from_secs(60);
const RESERVE_WAIT: Duration = Duration::from_secs(10);
/// Everything up to "slot reserved" (connect, handshake, reserve) for one job,
/// over all addresses and both targets (CF-27).
const OPEN_DEADLINE: Duration = Duration::from_secs(12);
/// Asking the farm for a cache entry (DESIGN section 16): the whole ask, the
/// peers asked, asks in flight, and the pause for a peer that did not answer.
const CACHE_ASK_DEADLINE: Duration = Duration::from_millis(300);
const CACHE_ASK_PEERS: usize = 8;
pub(crate) const CACHE_ASK_CONCURRENCY: usize = 16;
/// Entry downloads started at once, and how long one may wait to connect.
pub(crate) const CACHE_FETCH_CONCURRENCY: usize = 4;
const CACHE_FETCH_CONNECT: Duration = Duration::from_secs(3);
const CACHE_SLOW_PAUSE: Duration = Duration::from_secs(30);
/// The address that worked last time gets this head start over the others.
const ADDR_HEAD_START: Duration = Duration::from_millis(300);
/// Direct connects get at most this long when a dial-back can follow (CF-34).
const DIRECT_BUDGET: Duration = Duration::from_secs(4);
/// After no address of a peer answered, dial-backs come first for this long.
const DIRECT_RETRY_AFTER: Duration = Duration::from_secs(300);
/// A peer that could not be reached either way gets no jobs for this long.
const UNREACHABLE_PAUSE: Duration = Duration::from_secs(60);

/// The answer to `Reserve` on a fresh job connection.
enum Reservation {
    Ok(Link),
    Busy,
    Failed,
}

/// Removes a dial-back this node waits on when the waiter ends (answered,
/// timed out or cancelled), so tokens cannot pile up.
struct DialCleanup<'a> {
    node: &'a Node,
    token: [u8; 16],
}

impl Drop for DialCleanup<'_> {
    fn drop(&mut self) {
        lock(&self.node.dialbacks).remove(&self.token);
    }
}

/// Connects to several addresses at once and hands out the successes in the
/// order they finish. Dropping it cancels the connects that are still running.
pub(crate) struct Race<T> {
    set: tokio::task::JoinSet<Option<(String, T)>>,
    deadline: tokio::time::Instant,
}

impl<T: 'static> Race<T> {
    /// The next success, or `None` when all failed or the deadline passed.
    pub(crate) async fn next(&mut self) -> Option<(String, T)> {
        loop {
            match tokio::time::timeout_at(self.deadline, self.set.join_next()).await {
                Ok(Some(Ok(Some(found)))) => return Some(found),
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return None,
            }
        }
    }
}

/// Start `connect(addr)` for every address; the first address starts at once,
/// the rest after `others_delay`.
fn race_first<T, F, Fut>(
    addrs: Vec<String>,
    deadline: tokio::time::Instant,
    others_delay: Duration,
    connect: F,
) -> Race<T>
where
    T: Send + 'static,
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Option<T>> + Send + 'static,
{
    let mut set = tokio::task::JoinSet::new();
    for (i, a) in addrs.into_iter().enumerate() {
        let fut = connect(a.clone());
        let delay = if i == 0 { Duration::ZERO } else { others_delay };
        set.spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            fut.await.map(|t| (a, t))
        });
    }
    Race { set, deadline }
}
/// Uploads smaller than this say nothing about the link speed.
const MIN_MEASURED_UPLOAD: u64 = 2 * 1024 * 1024;
/// The whole blob upload of one job (CF-19). The per-frame waits inside it only
/// notice a peer that stops, not one that takes the slot for hours.
pub const UPLOAD_DEADLINE: Duration = Duration::from_secs(600);

/// What one remote job may stage on the submitter (CF-19): a peer that streams
/// endless or enormous outputs must not fill this disk. Beyond either limit
/// the job fails and runs locally.
pub const MAX_OUTPUT_FILES: usize = 10_000;
pub const MAX_OUTPUT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Running total of the outputs a job has announced so far.
#[derive(Default)]
struct OutputBudget {
    files: usize,
    bytes: u64,
}

impl OutputBudget {
    /// Count one announced output of `size` bytes; an error when the job goes
    /// over either limit.
    fn admit(&mut self, size: u64) -> anyhow::Result<()> {
        if self.files >= MAX_OUTPUT_FILES {
            bail!("peer sent more than {MAX_OUTPUT_FILES} output files");
        }
        let total = self.bytes.saturating_add(size);
        if total > MAX_OUTPUT_BYTES {
            bail!("peer sent more than {MAX_OUTPUT_BYTES} bytes of output");
        }
        self.files += 1;
        self.bytes = total;
        Ok(())
    }
}

/// Everything `Submit` carries besides the spec.
pub struct SubmitInfo {
    pub produces: Vec<ClientPath>,
    pub externs: Vec<ClientPath>,
    pub input: ClientPath,
    pub search_dirs: Vec<(String, ClientPath)>,
    pub link_libs: Vec<(String, String)>,
}

struct Plan {
    spec: JobSpec,
    /// Peers to try, best first (at most two are tried).
    targets: Vec<NodeId>,
    /// Local file for every blob the peer may ask for.
    files: HashMap<BlobHash, PathBuf>,
    /// Released once the peer confirmed the reservation (its own status then counts the slot).
    pending: std::sync::Mutex<Option<PendingGuard>>,
}

/// Counts a reservation we are about to make, so concurrent submits do not
/// all pick the same peer before its next status arrives.
struct PendingGuard {
    node: Arc<Node>,
    peer: NodeId,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(p) = lock(&self.node.peers).get_mut(&self.peer) {
            p.pending = p.pending.saturating_sub(1);
        }
    }
}

struct RunningGuard {
    node: Arc<Node>,
    remote: bool,
}

impl RunningGuard {
    fn new(node: &Arc<Node>, remote: bool) -> RunningGuard {
        let c = if remote {
            &node.running_remote
        } else {
            &node.running_local
        };
        c.fetch_add(1, Ordering::Relaxed);
        node.status_changed();
        RunningGuard {
            node: node.clone(),
            remote,
        }
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        let c = if self.remote {
            &self.node.running_remote
        } else {
            &self.node.running_local
        };
        c.fetch_sub(1, Ordering::Relaxed);
        self.node.status_changed();
    }
}

/// How long a job link may stay without a frame from the peer, outside blob
/// transfers (CF-25). The executor sends a keepalive every 5 s, so this is six
/// missed beats.
pub const PEER_FRAME_IDLE: Duration = Duration::from_secs(30);
/// Time beyond `job_timeout_min` the whole remote attempt may take: upload,
/// output transfer and the executor's own timeout handling.
const JOB_SLACK: Duration = Duration::from_secs(120);

/// What the node knows about the liveness of the work behind one `Submit`.
/// The keepalive to the wrapper is sent only while this says the job is alive,
/// so a wrapper never mistakes "the node is up" for "my job is progressing".
struct Liveness {
    base: Instant,
    last_ms: AtomicU64,
    /// A remote attempt is in flight (waiting on a peer).
    remote: AtomicBool,
    /// The wrapper was told to run rustc itself: nothing more to say to it.
    stopped: AtomicBool,
}

impl Liveness {
    fn new() -> Arc<Liveness> {
        Arc::new(Liveness {
            base: Instant::now(),
            last_ms: AtomicU64::new(0),
            remote: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        })
    }

    /// Progress of the peer: a frame came from it, or one went to it.
    fn touch(&self) {
        self.last_ms
            .store(self.base.elapsed().as_millis() as u64, Ordering::Release);
    }

    fn since_touch(&self) -> Duration {
        let last = self.last_ms.load(Ordering::Acquire);
        Duration::from_millis((self.base.elapsed().as_millis() as u64).saturating_sub(last))
    }

    /// Mark the start or end of a remote attempt.
    fn set_remote(&self, on: bool) {
        self.touch();
        self.remote.store(on, Ordering::Release);
    }

    /// Whether the wrapper should hear a keepalive now.
    fn keepalive_due(&self) -> bool {
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        !self.remote.load(Ordering::Acquire) || self.since_touch() < PEER_FRAME_IDLE
    }
}

/// Marks a remote attempt for the length of its scope.
struct RemoteScope<'a>(&'a Liveness);

impl<'a> RemoteScope<'a> {
    fn new(live: &'a Liveness) -> Self {
        live.set_remote(true);
        RemoteScope(live)
    }
}

impl Drop for RemoteScope<'_> {
    fn drop(&mut self) {
        self.0.set_remote(false);
    }
}

/// How long the node listens for the wrapper's end-of-file before it commits
/// anything to the client's directories (CF-25).
const WRAPPER_CHECK: Duration = Duration::from_millis(10);

/// What a finished remote job or cache replay hands back (DESIGN section 16).
#[derive(Default)]
struct DoneInfo {
    /// The result, ready to become a cache entry once the wrapper has accepted it.
    capture: Option<Captured>,
    /// Output bytes that were applied.
    bytes: u64,
}

/// Where the frames of a run come from.
enum Source<'a> {
    /// A job on a peer.
    Job { plan: &'a Plan, target: NodeId },
    /// A cache entry, replayed as if a peer had run the job.
    Cache(Origin),
}

/// Whose cache a hit came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Local,
    Farm,
}

/// A job whose result may enter the compile cache: its key and the spec the
/// key was made from (with the inputs).
struct CacheCtx {
    key: CacheKey,
    spec: JobSpec,
    /// A LOCAL run's result of this crate may be stored (DESIGN section 16).
    /// True only when no proc macro is in the crate's `--extern` closure:
    /// rustc then reads only what the key covers (the dep-info is checked
    /// before storing, the variables by the wrapper). A proc macro is native
    /// code that can read any file, so with one in the closure only a REMOTE
    /// result is kept, because the peer's sandbox held nothing but the
    /// shipped inputs.
    local_ok: bool,
}

enum RemoteEnd {
    /// The wrapper closed its connection (it fell back to a local rustc after
    /// the node looked silent) or the node is stopping: no output of this job
    /// is written, the peer was told to stop.
    WrapperGone,
    /// The final `Exit` was relayed to the wrapper.
    Done(DoneInfo),
    /// Nothing usable came back; run locally. The text is for debug logs.
    Fallback(String),
    /// An output embeds the peer's sandbox path (CF-10): nothing was written;
    /// run locally and count it as `SandboxPathInOutput`.
    Tainted,
}

/// A job's output may only land in its `--out-dir` (or be one of the files the
/// invocation announced); anything else is a peer trying to place files where
/// the submitter never asked for them.
pub fn output_allowed(out_dir: &ClientPath, produces: &[ClientPath], p: &ClientPath) -> bool {
    // Hand-built paths (`..` components, relative strings) fail this round trip.
    if ClientPath::parse(p.style, &p.path).as_ref() != Some(p) {
        return false;
    }
    if p.style != out_dir.style {
        return false;
    }
    (p != out_dir && p.starts_with(out_dir)) || produces.contains(p)
}

/// True when `path` lies under one of the configured prefixes.
pub fn under_prefix(prefixes: &[String], path: &ClientPath) -> bool {
    prefixes
        .iter()
        .filter_map(|p| ClientPath::parse(path.style, p))
        .any(|p| path.starts_with(&p))
}

/// Write `data` next to `dest` into a temporary file and return its path.
fn write_temp(dest: &std::path::Path, data: &[u8]) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    let parent = dest.parent().context("output path has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let mut tag = [0u8; 6];
    rand::fill(&mut tag[..]);
    let tmp = parent.join(format!(
        ".crabfarm-{}.tmp",
        data_encoding::HEXLOWER.encode(&tag)
    ));
    if let Err(e) = std::fs::write(&tmp, data) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("write {}", tmp.display()));
    }
    Ok(tmp)
}

/// Move a temporary file onto its destination.
fn commit_temp(tmp: &std::path::Path, dest: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    if let Err(e) = std::fs::rename(tmp, dest) {
        let _ = std::fs::remove_file(tmp);
        return Err(e).with_context(|| format!("replace {}", dest.display()));
    }
    Ok(())
}

/// The `message` of a rustc JSON diagnostic whose level is `error`, or the text
/// of a plain `error...` line; `None` for everything else.
fn first_error_message(line: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
        return (v.get("level")?.as_str()? == "error")
            .then(|| v.get("message")?.as_str().map(str::to_string))
            .flatten();
    }
    line.starts_with("error").then(|| line.to_string())
}

/// Write `data` to `dest` through a temporary file in the same directory.
#[cfg(test)]
fn write_atomic(dest: &std::path::Path, data: &[u8]) -> anyhow::Result<()> {
    commit_temp(&write_temp(dest, data)?, dest)
}

/// Outputs of a remote job that arrived but are not applied yet (CF-10): the
/// job may turn out to be tainted (the peer found its sandbox path in an
/// output), and then none of them may reach the client's target directory.
/// Whatever is still staged when this is dropped is deleted.
#[derive(Default)]
struct Staged {
    files: Vec<(ClientPath, PathBuf, PathBuf)>,
}

impl Staged {
    fn add(&mut self, client: ClientPath, tmp: PathBuf, dest: PathBuf) {
        self.files.push((client, tmp, dest));
    }

    /// Move every staged file to its destination.
    fn commit(&mut self) -> anyhow::Result<()> {
        while !self.files.is_empty() {
            let (_, tmp, dest) = self.files.remove(0);
            commit_temp(&tmp, &dest)?;
        }
        Ok(())
    }

    /// Move the one staged file for `client` to its destination; false when
    /// nothing is staged for that path.
    fn commit_one(&mut self, client: &ClientPath) -> anyhow::Result<bool> {
        let Some(i) = self.files.iter().position(|(c, _, _)| c == client) else {
            return Ok(false);
        };
        let (_, tmp, dest) = self.files.remove(i);
        commit_temp(&tmp, &dest)?;
        Ok(true)
    }

    fn discard(&mut self) {
        for (_, tmp, _) in self.files.drain(..) {
            let _ = std::fs::remove_file(tmp);
        }
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        self.discard();
    }
}

/// Artifact kinds that are relayed the moment their file is clean (CF-16):
/// the metadata is what lets cargo start the dependents of a crate before its
/// codegen is done, and dep-info lets the wrapper judge env dependencies early.
/// Everything else (`link`, `obj`, ...) waits for `Exit` like the other files.
pub(crate) fn is_early_emit(emit: &str) -> bool {
    matches!(emit, "metadata" | "dep-info")
}

/// Decides, per frame of a remote job, what the wrapper sees now and what
/// waits for `Exit` (DESIGN section 9, CF-10 and CF-16).
///
/// Files are staged next to their destination. A clean early artifact (the
/// executor scanned it before sending it) is moved into place when its
/// notification line arrives, and the line is relayed at once, so cargo starts
/// dependents early. From the first notice that is not early on (the link
/// artifact), all frames wait for `Exit`, which keeps the order of the
/// diagnostics and keeps cargo from seeing a notice before its file exists.
#[derive(Default)]
struct Relay {
    staged: Staged,
    held: Vec<Message>,
    holding: bool,
    /// (file, emit) of every artifact notice seen: rustc says each once, and
    /// cargo starts a crate's dependents on the metadata notice, so a peer that
    /// repeats one must not make cargo see it twice (CF-25).
    notices: HashSet<(String, String)>,
}

impl Relay {
    /// A frame from the peer for the wrapper: `Some((frame, early))` when it
    /// goes out now (`early` for an artifact notice relayed before `Exit`),
    /// `None` when it was held.
    fn offer(
        &mut self,
        m: Message,
        style: crate::proto::PathStyle,
    ) -> anyhow::Result<Option<(Message, bool)>> {
        let notice = match &m {
            Message::Stderr { line } => crate::rewrite::artifact(line),
            _ => None,
        };
        if let Some(key) = &notice
            && !self.notices.insert(key.clone())
        {
            return Ok(None);
        }
        if self.holding {
            self.held.push(m);
            return Ok(None);
        }
        if let Some((file, emit)) = notice {
            let ready = match ClientPath::parse(style, &file) {
                Some(client) if is_early_emit(&emit) => self.staged.commit_one(&client)?,
                _ => false,
            };
            if !ready {
                self.holding = true;
                self.held.push(m);
                return Ok(None);
            }
            return Ok(Some((m, true)));
        }
        Ok(Some((m, false)))
    }

    /// The peer found its sandbox path in an output: use none of the staged
    /// files and relay nothing more. Files committed early stay; the local
    /// rerun overwrites them (DESIGN section 9).
    fn taint(&mut self) {
        self.staged.discard();
        self.held.clear();
    }

    /// `Exit` arrived: apply the remaining files (only for a success), then
    /// hand back the held frames, which announce them.
    fn finish(&mut self, ok: bool) -> anyhow::Result<Vec<Message>> {
        if ok {
            self.staged.commit()?;
        } else {
            self.staged.discard();
        }
        Ok(std::mem::take(&mut self.held))
    }
}

fn env_path(spec: &JobSpec, name: &str) -> Option<ClientPath> {
    spec.env
        .iter()
        .find(|(k, _)| k == name)
        .and_then(|(_, v)| match v {
            EnvValue::Path(p) => Some(p.clone()),
            EnvValue::Plain(_) => None,
        })
}

/// A job with its inputs collected.
struct Prepared {
    spec: JobSpec,
    files: HashMap<BlobHash, PathBuf>,
    /// Content hashes of the proc-macro libraries in the `--extern` closure.
    macros: Vec<BlobHash>,
}

/// Whether the result of a local run is kept for the compile cache, by the
/// reason it ran locally. A crate that failed on a peer but built here, or
/// whose result the wrapper rejected, depends on something the job did not
/// carry (a file outside the package directory read by a proc macro, a
/// variable): exactly the crates the cache must not hold (DESIGN section 16).
fn stores_local_result(reason: LocalReason) -> bool {
    !matches!(
        reason,
        LocalReason::RemoteCompileErrorRetry | LocalReason::EnvDependency
    )
}

/// The primary input file must be one of the hashed inputs: a source outside
/// the package directory is not in the key (DESIGN section 16).
fn covers_input(spec: &JobSpec, input: &ClientPath) -> bool {
    spec.inputs.iter().any(|i| i.path == *input)
}

struct Built {
    inputs: Vec<InputFile>,
    files: HashMap<BlobHash, PathBuf>,
    has_proc_macro: bool,
    macros: Vec<BlobHash>,
}

/// Collect and hash everything the job needs (DESIGN section 8).
fn build_inputs(node: &Node, spec: &JobSpec, info: &SubmitInfo) -> Result<Built, LocalReason> {
    let pkg = env_path(spec, "CARGO_MANIFEST_DIR")
        .map(|p| p.to_local())
        .or_else(|| info.input.to_local().parent().map(PathBuf::from))
        .ok_or(LocalReason::NotEligible)?;
    let out_dir_env = env_path(spec, "OUT_DIR").map(|p| p.to_local());
    let search: Vec<(String, PathBuf)> = info
        .search_dirs
        .iter()
        .filter(|(k, _)| k == "native" || k == "all")
        .map(|(k, p)| (k.clone(), p.to_local()))
        .collect();
    let collected = inputs::collect(
        &pkg,
        out_dir_env.as_deref(),
        &search,
        &info.link_libs,
        spec.target.contains("msvc"),
    )
    .map_err(|e| {
        tracing::debug!(
            crate_name = spec.crate_name.as_str(),
            "input collection: {e}"
        );
        match e {
            CollectError::TooLarge => LocalReason::TooLarge,
            _ => LocalReason::NotEligible,
        }
    })?;

    let closure = lock(&node.depgraph).closure(&info.externs);
    let closure = closure.map_err(|e| {
        tracing::debug!(crate_name = spec.crate_name.as_str(), "{e}");
        // Which of the two (CF-35): `crabfarm status` and `doctor` explain it.
        node.count(
            match e {
                crate::depgraph::Unknown::NotRecorded(_) => "unknown_dep.not_recorded",
                crate::depgraph::Unknown::Changed(_) => "unknown_dep.changed",
            },
            1,
        );
        LocalReason::UnknownDependency
    })?;
    let has_proc_macro = closure.iter().any(is_proc_macro_lib);
    let macro_paths: Vec<ClientPath> = closure
        .iter()
        .filter(|c| is_proc_macro_lib(c))
        .cloned()
        .collect();

    let mut seen: HashSet<ClientPath> = HashSet::new();
    let mut inputs_out: Vec<InputFile> = Vec::new();
    let mut files: HashMap<BlobHash, PathBuf> = HashMap::new();
    let mut total = collected.total;
    let mut add = |path: PathBuf, executable: bool| -> Result<(), LocalReason> {
        let client = ClientPath::from_local(&path, std::path::Path::new("."))
            .ok_or(LocalReason::NotEligible)?;
        if !seen.insert(client.clone()) {
            return Ok(());
        }
        let (hash, size) = lock(&node.hashes)
            .hash(&client, &path)
            .map_err(|_| LocalReason::NotEligible)?;
        inputs_out.push(InputFile {
            path: client,
            hash,
            size,
            executable,
        });
        files.entry(hash).or_insert(path);
        Ok(())
    };
    for (path, _size, exec) in collected.files {
        add(path, exec)?;
    }
    for c in closure {
        let local = c.to_local();
        total = total.saturating_add(
            std::fs::metadata(&local)
                .map(|m| m.len())
                .map_err(|_| LocalReason::UnknownDependency)?,
        );
        if total > inputs::MAX_TOTAL {
            return Err(LocalReason::TooLarge);
        }
        add(local, false)?;
    }
    let mut macros: Vec<BlobHash> = inputs_out
        .iter()
        .filter(|i| macro_paths.contains(&i.path))
        .map(|i| i.hash)
        .collect();
    macros.sort_by_key(|h| h.0);
    macros.dedup();
    Ok(Built {
        inputs: inputs_out,
        files,
        has_proc_macro,
        macros,
    })
}

/// `link.send`, but giving up at `deadline`: a frame waits for room in the
/// queue while the peer does not read, and that must not last for ever.
async fn send_by(link: &Link, m: Message, deadline: Instant) -> anyhow::Result<()> {
    match tokio::time::timeout_at(deadline.into(), link.send(m)).await {
        Ok(r) => r,
        Err(_) => bail!("blob upload exceeded its overall deadline"),
    }
}

/// The next frame of a job link. Fails when the peer sent nothing for
/// [`PEER_FRAME_IDLE`] (its executor beats every 5 s, so that peer is gone), when
/// the link closed, or when `deadline` (the whole job) has passed.
async fn next_frame(link: &mut Link, deadline: Instant) -> anyhow::Result<Message> {
    next_frame_within(link, deadline, PEER_FRAME_IDLE).await
}

async fn next_frame_within(
    link: &mut Link,
    deadline: Instant,
    idle: Duration,
) -> anyhow::Result<Message> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        bail!("remote job exceeded its overall deadline");
    }
    match tokio::time::timeout(idle.min(left), link.recv()).await {
        Ok(Some(m)) => Ok(m),
        Ok(None) => bail!("connection to the peer lost"),
        Err(_) if Instant::now() >= deadline => bail!("remote job exceeded its overall deadline"),
        Err(_) => bail!("peer silent"),
    }
}

struct PeerView {
    id: NodeId,
    free: u32,
    speed: u32,
    bandwidth: u64,
}

impl Node {
    // ------------------------------------------------------------- IPC loop

    /// Serve one authenticated IPC connection.
    pub async fn handle_ipc(self: Arc<Self>, mut link: Link) {
        while let Some(m) = link.recv().await {
            match m {
                Message::QueryStatus => {
                    let reply = Message::FarmStatus {
                        local: self.own_status(),
                        peers: self.peer_infos_for_status(),
                        counters: self.counters_snapshot(),
                    };
                    if link.send(reply).await.is_err() {
                        break;
                    }
                }
                Message::Shutdown => {
                    // The sender is authenticated by the IPC token, which only
                    // the owner of the data directory can read.
                    tracing::info!("stop requested over local IPC");
                    self.shutdown.cancel();
                    break;
                }
                Message::Submit {
                    spec,
                    rustc: _,
                    produces,
                    externs,
                    input,
                    search_dirs,
                    link_libs,
                    force_local,
                } => {
                    let info = SubmitInfo {
                        produces,
                        externs,
                        input,
                        search_dirs,
                        link_libs,
                    };
                    // The wrapper tells a busy node from a dead one by these
                    // (CF-19). Input collection, a wait for a peer or for a local
                    // slot can all take a while without any other frame.
                    //
                    // Not blindly: while a peer runs the job, a keepalive means
                    // "the job is alive" (a frame came from the peer in the last
                    // 30 s), so a dead peer reaches the wrapper's own fallback
                    // instead of being papered over (CF-25).
                    let live = Liveness::new();
                    let gate = live.clone();
                    let _ticker = link
                        .keepalive_ticker(crate::ipc::KEEPALIVE_INTERVAL, move || {
                            gate.keepalive_due()
                        });
                    self.handle_submit(&mut link, spec, info, force_local, &live)
                        .await;
                    break;
                }
                _ => break,
            }
        }
        link.close().await;
    }

    // --------------------------------------------------------------- submit

    async fn handle_submit(
        self: &Arc<Self>,
        wrapper: &mut Link,
        spec: JobSpec,
        info: SubmitInfo,
        force_local: bool,
        live: &Liveness,
    ) {
        lock(&self.depgraph).begin(&info.produces, &info.externs);
        if force_local {
            // The wrapper knows a variable this crate reads is not forwarded
            // (CF-18): no peer is tried, no inputs are collected, and nothing
            // of this run may enter the compile cache (the variable is not in
            // its key).
            self.local_phase(wrapper, LocalReason::EnvDependency, &info, live, None)
                .await;
            return;
        }
        // Compile cache (DESIGN section 16): the inputs are collected before
        // anything else, because the key needs their hashes, and a hit is the
        // cheapest way to run the job.
        let mut prepared: Option<Result<Prepared, LocalReason>> = None;
        let mut ctx: Option<Arc<CacheCtx>> = None;
        // Only a job the cache can use is prepared here (a target given as a
        // file is never cached): everything else is prepared by `plan`, and
        // only when a peer looks better than a free local slot.
        if self.cache_enabled()
            && !self.paths_forbid_cache(&spec, &info)
            && self.target_cacheable(&spec).await
        {
            let r = self.prepare(spec.clone(), &info, false).await;
            if let Ok(p) = &r
                && covers_input(&p.spec, &info.input)
                && !ccache::target_is_file(&p.spec.target)
            {
                ctx = Some(Arc::new(CacheCtx {
                    key: ccache::cache_key(&p.spec),
                    local_ok: p.macros.is_empty(),
                    spec: p.spec.clone(),
                }));
            }
            prepared = Some(r);
        }
        if let Some(c) = &ctx
            && self.try_cache(wrapper, &info, c, live).await
        {
            return;
        }
        let plan = match self.plan(spec, &info, prepared).await {
            Ok(p) => p,
            Err(reason) => {
                self.local_phase(wrapper, reason, &info, live, ctx.as_deref())
                    .await;
                return;
            }
        };
        match self
            .run_remote(wrapper, &plan, &info, ctx.is_some(), live)
            .await
        {
            RemoteEnd::Done(done) => {
                self.after_done(wrapper, &info, live, ctx.as_deref(), done, None)
                    .await;
            }
            RemoteEnd::Fallback(why) => {
                // Info, not debug: a fallback is counted and its reason is what
                // anyone investigating `RemoteFailed` needs (CF-27).
                tracing::info!(
                    crate_name = plan.spec.crate_name.as_str(),
                    "remote job failed: {why}"
                );
                self.count("remote_failed", 1);
                self.local_phase(
                    wrapper,
                    LocalReason::RemoteFailed,
                    &info,
                    live,
                    ctx.as_deref(),
                )
                .await;
            }
            RemoteEnd::WrapperGone => {
                tracing::debug!(
                    crate_name = plan.spec.crate_name.as_str(),
                    "the wrapper went local, outputs of the remote job dropped"
                );
                self.count("wrapper_gone", 1);
                lock(&self.depgraph).finish(&info.produces, false);
            }
            RemoteEnd::Tainted => {
                tracing::info!(
                    crate_name = plan.spec.crate_name.as_str(),
                    "remote output embeds the sandbox path, rerunning locally"
                );
                self.local_phase(
                    wrapper,
                    LocalReason::SandboxPathInOutput,
                    &info,
                    live,
                    ctx.as_deref(),
                )
                .await;
            }
        }
    }

    /// The wrapper has its answer. It speaks again only to reject the result
    /// (compile error of a non-registry crate, env-dep mismatch): then it
    /// waits for a local slot. Its closing the connection is the acceptance:
    /// only now is a hit counted and a fresh result stored.
    async fn after_done(
        self: &Arc<Self>,
        wrapper: &mut Link,
        info: &SubmitInfo,
        live: &Liveness,
        ctx: Option<&CacheCtx>,
        done: DoneInfo,
        hit: Option<Origin>,
    ) {
        match tokio::time::timeout(RETRY_WAIT, wrapper.recv()).await {
            Ok(Some(Message::RetryLocal { reason })) => {
                // It waits for a slot now and needs to hear that we live.
                live.stopped.store(false, Ordering::Release);
                if hit.is_some() {
                    self.count("cache_rejected", 1);
                }
                // The wrapper found the replayed result unfit (a variable the
                // crate reads): this node's entry must not serve it again.
                if hit == Some(Origin::Local)
                    && let Some(c) = ctx
                {
                    let (node, key) = (self.clone(), c.key);
                    let _ = tokio::task::spawn_blocking(move || {
                        if node.ccache.as_ref().is_some_and(|cc| cc.remove(&key)) {
                            node.count("cache_rejected_removed", 1);
                        }
                    })
                    .await;
                }
                self.local_phase(wrapper, reason, info, live, ctx).await;
            }
            Ok(None) => {
                match hit {
                    Some(Origin::Local) => self.count("cache_hit_local", 1),
                    Some(Origin::Farm) => self.count("cache_hit_farm", 1),
                    None => {}
                }
                if hit.is_some() {
                    self.count("cache_bytes_saved", done.bytes);
                }
                // A local hit is stored already; everything else is new here.
                if hit != Some(Origin::Local)
                    && let (Some(c), Some(cap)) = (ctx, done.capture)
                {
                    self.store_capture(c, cap).await;
                }
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------------- cache

    pub(crate) fn cache_enabled(&self) -> bool {
        self.ccache.is_some() && self.cfg.retain_blobs
    }

    /// Only a built-in target is cached: a target specification file (or any
    /// custom name) has content the key does not see (CF-28 R2). Asks the
    /// job's toolchain once for `--print target-list`.
    async fn target_cacheable(self: &Arc<Self>, spec: &JobSpec) -> bool {
        let (node, id, target) = (self.clone(), spec.toolchain.clone(), spec.target.clone());
        tokio::task::spawn_blocking(move || node.toolchains.is_builtin_target(&id, &target))
            .await
            .unwrap_or(false)
    }

    /// `local_only` and `sensitive` paths never enter the cache, in either
    /// direction (DESIGN sections 10 and 16).
    fn paths_forbid_cache(&self, spec: &JobSpec, info: &SubmitInfo) -> bool {
        let manifest = env_path(spec, "CARGO_MANIFEST_DIR");
        let mut roots = vec![&info.input];
        roots.extend(manifest.as_ref());
        roots
            .iter()
            .any(|p| under_prefix(&self.cfg.local_only, p) || under_prefix(&self.cfg.sensitive, p))
    }

    /// Look the job up in this node's cache, then in the farm's. True when a
    /// hit answered the wrapper (it may still ask for a local run afterwards).
    async fn try_cache(
        self: &Arc<Self>,
        wrapper: &mut Link,
        info: &SubmitInfo,
        ctx: &Arc<CacheCtx>,
        live: &Liveness,
    ) -> bool {
        let node = self.clone();
        let key = ctx.key;
        let local = tokio::task::spawn_blocking(move || {
            let cc = node.ccache.as_ref()?;
            let e = cc.lookup(&key)?;
            // Everything is read and verified before the first frame goes out.
            match cc.load(&e) {
                Ok(files) => Some((e, files)),
                Err(err) => {
                    tracing::info!("cache entry unusable, running normally: {err:#}");
                    cc.forget(&key, &e);
                    node.count("cache_corrupt", 1);
                    None
                }
            }
        })
        .await
        .ok()
        .flatten();
        if let Some((entry, files)) = local {
            let (a, b) = tokio::io::duplex(1 << 20);
            let feeder = tokio::spawn(async move {
                let link = Link::new(crate::transport::FramedConn::new(a));
                let _ = crate::executor::replay_entry(&link, &entry, &files).await;
                link.close().await;
            });
            let link = Link::new(crate::transport::FramedConn::new(b));
            let end = self
                .drive_remote(
                    wrapper,
                    link,
                    &ctx.spec,
                    info,
                    Source::Cache(Origin::Local),
                    false,
                    live,
                )
                .await;
            feeder.abort();
            if self
                .finish_hit(wrapper, info, live, ctx, end, Origin::Local)
                .await
            {
                return true;
            }
        }
        if let Some(link) = self.farm_lookup(&ctx.key).await {
            let end = self
                .drive_remote(
                    wrapper,
                    link,
                    &ctx.spec,
                    info,
                    Source::Cache(Origin::Farm),
                    true,
                    live,
                )
                .await;
            if self
                .finish_hit(wrapper, info, live, ctx, end, Origin::Farm)
                .await
            {
                return true;
            }
        }
        self.count("cache_miss", 1);
        tracing::debug!(crate_name = ctx.spec.crate_name.as_str(), "cache miss");
        false
    }

    /// After a replay: true when the wrapper was answered (or is gone).
    async fn finish_hit(
        self: &Arc<Self>,
        wrapper: &mut Link,
        info: &SubmitInfo,
        live: &Liveness,
        ctx: &CacheCtx,
        end: RemoteEnd,
        origin: Origin,
    ) -> bool {
        match end {
            RemoteEnd::Done(done) => {
                self.after_done(wrapper, info, live, Some(ctx), done, Some(origin))
                    .await;
                true
            }
            RemoteEnd::WrapperGone => {
                self.count("wrapper_gone", 1);
                lock(&self.depgraph).finish(&info.produces, false);
                true
            }
            RemoteEnd::Fallback(why) => {
                // Nothing is applied before every frame is known good, so the
                // job simply runs the normal way.
                tracing::info!(
                    crate_name = ctx.spec.crate_name.as_str(),
                    "cache entry not used: {why}"
                );
                self.count("cache_error", 1);
                false
            }
            RemoteEnd::Tainted => false,
        }
    }

    /// Keep a result that came from a peer (or from a farm hit) as a local entry.
    async fn store_capture(self: &Arc<Self>, ctx: &CacheCtx, cap: Captured) {
        let node = self.clone();
        let (key, spec) = (ctx.key, ctx.spec.clone());
        let stored = tokio::task::spawn_blocking(move || {
            let cc = node.ccache.as_ref()?;
            let e = ccache::entry_from(&spec, &key, cap, ccache::Ccache::now())?;
            cc.store(&key, &e).ok()
        })
        .await
        .ok()
        .flatten();
        if stored.is_some() {
            self.count("cache_stored", 1);
        } else {
            tracing::debug!("cache: remote result not stored");
        }
    }

    /// Store what a LOCAL rustc run left behind. Only when the inputs are still
    /// what the key says (the user may have saved a file while rustc ran).
    async fn store_local(
        self: &Arc<Self>,
        ctx: &CacheCtx,
        info: &SubmitInfo,
        capture: crate::proto::LocalCapture,
        started: std::time::SystemTime,
    ) {
        let node = self.clone();
        let key = ctx.key;
        let spec = ctx.spec.clone();
        let produces = info.produces.clone();
        let stored = tokio::task::spawn_blocking(move || {
            let cc = node.ccache.as_ref()?;
            for i in &spec.inputs {
                let now = lock(&node.hashes).hash(&i.path, &i.path.to_local()).ok()?;
                if now.0 != i.hash {
                    return None;
                }
            }
            let plain: Vec<String> = spec
                .args
                .iter()
                .filter_map(|a| match a {
                    crate::proto::Arg::Plain(s) => Some(s.clone()),
                    _ => None,
                })
                .collect();
            let extra = crate::wrapper::extra_filename(&plain);
            let files = ccache::local_output_files(&spec, &produces, &extra, started)?;
            let mut outputs = Vec::new();
            let mut dep_info = None;
            let mut total = 0u64;
            if files.len() > ccache::MAX_ENTRY_FILES {
                return None;
            }
            for f in files {
                // What a replay could never carry is refused by its size,
                // before a byte of it is read into memory (R7).
                let room = ccache::MAX_ENTRY_BYTES
                    .saturating_sub(total)
                    .min(ccache::MAX_ENTRY_FILE_BYTES);
                let data = ccache::read_capped(&f.to_local(), room)?;
                total += data.len() as u64;
                let hash = BlobHash::of(&data);
                cc.put_blob(&hash, &data).ok()?;
                let size = data.len() as u64;
                if f.path.ends_with(".d") {
                    dep_info = String::from_utf8(data).ok();
                }
                outputs.push(EntryOutput {
                    path: f,
                    hash,
                    size,
                });
            }
            let e = ccache::entry_from(
                &spec,
                &key,
                Captured {
                    outputs,
                    lines: capture.lines,
                    dep_info,
                },
                ccache::Ccache::now(),
            )?;
            cc.store(&key, &e).ok()
        })
        .await
        .ok()
        .flatten();
        if stored.is_some() {
            self.count("cache_stored", 1);
        } else {
            tracing::debug!("cache: local result not stored");
        }
    }

    /// Wait for a local slot (FIFO), hand it to the wrapper, and wait until its
    /// rustc is done.
    async fn local_phase(
        self: &Arc<Self>,
        wrapper: &mut Link,
        reason: LocalReason,
        info: &SubmitInfo,
        live: &Liveness,
        ctx: Option<&CacheCtx>,
    ) {
        self.count_local(reason);
        lock(&self.depgraph).begin(&info.produces, &info.externs);
        let Ok(permit) = self.local_sem.clone().acquire_owned().await else {
            return;
        };
        let _running = RunningGuard::new(self, false);
        if wrapper.send(Message::RunLocal { reason }).await.is_err() {
            lock(&self.depgraph).finish(&info.produces, false);
            return;
        }
        // The wrapper reads nothing more until it is done with rustc.
        live.stopped.store(true, Ordering::Release);
        // Files older than this are not outputs of this run.
        let started = std::time::SystemTime::now();
        let mut capture = None;
        let ok = loop {
            match wrapper.recv().await {
                Some(Message::LocalDone {
                    code, capture: c, ..
                }) => {
                    capture = c;
                    break code == 0;
                }
                Some(_) => continue,
                None => break false,
            }
        };
        lock(&self.depgraph).finish(&info.produces, ok);
        drop(permit);
        if ok && let (Some(c), Some(cap)) = (ctx, capture) {
            if c.local_ok && stores_local_result(reason) {
                self.store_local(c, info, cap, started).await;
            } else {
                // Either a proc macro ran here and may have read files the key
                // does not cover (only a remote result is kept, section 16),
                // or the crate needed something the job did not carry.
                self.count("cache_local_skipped", 1);
            }
        }
    }

    // ----------------------------------------------------------------- plan

    /// Eligible peers with a free slot, and whether any peer was eligible at all.
    fn peer_views(&self, spec: &JobSpec) -> (Vec<PeerView>, bool) {
        let needs = JobNeeds {
            toolchain: &spec.toolchain,
            target: &spec.target,
            needs_host: spec.needs_host,
            local_host: &self.host_triple,
        };
        let mut any = false;
        let mut views = Vec::new();
        let now = Instant::now();
        for (id, p) in lock(&self.peers).iter() {
            let Some(st) = &p.status else { continue };
            if p.conns == 0 || !scheduler::eligible(st, &needs) {
                continue;
            }
            // Jobs could not reach it either way a moment ago (CF-34).
            if p.unreachable_until.is_some_and(|t| t > now) {
                continue;
            }
            any = true;
            let free = st
                .slots_total
                .saturating_sub(st.slots_busy)
                .saturating_sub(p.pending);
            if free > 0 {
                views.push(PeerView {
                    id: *id,
                    free,
                    speed: st.speed.max(1),
                    bandwidth: p.bandwidth_bps,
                });
            }
        }
        // Stable order for equal scores.
        views.sort_by_key(|v| v.id);
        (views, any)
    }

    /// Collect and hash the inputs of a job and complete its spec with them
    /// (DESIGN section 8). Shared by the scheduler and the compile cache, whose
    /// key needs the same hashes.
    async fn prepare(
        self: &Arc<Self>,
        mut spec: JobSpec,
        info: &SubmitInfo,
        sensitive: bool,
    ) -> Result<Prepared, LocalReason> {
        // Inputs cost disk and CPU.
        let _prep = self
            .prep_sem
            .acquire()
            .await
            .map_err(|_| LocalReason::NotEligible)?;
        let node = self.clone();
        let spec_for_build = spec.clone();
        let info_for_build = SubmitInfo {
            produces: Vec::new(),
            externs: info.externs.clone(),
            input: info.input.clone(),
            search_dirs: info.search_dirs.clone(),
            link_libs: info.link_libs.clone(),
        };
        let built = tokio::task::spawn_blocking(move || {
            build_inputs(&node, &spec_for_build, &info_for_build)
        })
        .await
        .map_err(|_| LocalReason::NotEligible)??;

        spec.needs_host |= built.has_proc_macro;
        spec.no_retain = !self.cfg.retain_blobs || sensitive;
        let mut dirs: Vec<ClientPath> = vec![spec.out_dir.clone()];
        dirs.extend(info.search_dirs.iter().map(|(_, p)| p.clone()));
        dirs.extend(env_path(&spec, "OUT_DIR"));
        dirs.sort();
        dirs.dedup();
        spec.dirs = dirs;
        spec.inputs = built.inputs;
        Ok(Prepared {
            spec,
            files: built.files,
            macros: built.macros,
        })
    }

    async fn plan(
        self: &Arc<Self>,
        spec: JobSpec,
        info: &SubmitInfo,
        prepared: Option<Result<Prepared, LocalReason>>,
    ) -> Result<Plan, LocalReason> {
        // DESIGN section 10: configured prefixes never leave the machine.
        let manifest = env_path(&spec, "CARGO_MANIFEST_DIR");
        let mut roots = vec![&info.input];
        roots.extend(manifest.as_ref());
        if roots.iter().any(|p| under_prefix(&self.cfg.local_only, p)) {
            return Err(LocalReason::LocalOnlyPath);
        }
        let sensitive = roots.iter().any(|p| under_prefix(&self.cfg.sensitive, p));

        let local_free = self.local_sem.available_permits() as u32;
        let (views, any) = self.peer_views(&spec);
        if views.is_empty() {
            return Err(if any {
                LocalReason::PeersBusy
            } else {
                LocalReason::NoPeer
            });
        }
        let cands: Vec<Candidate> = views
            .iter()
            .map(|v| Candidate {
                free_slots: v.free,
                speed: v.speed,
                upload_bytes: 0,
                bandwidth_bps: v.bandwidth,
            })
            .collect();
        if scheduler::choose(local_free, self.cfg.prefer_remote, &cands) == Choice::Local {
            return Err(LocalReason::LocalSlotFree);
        }

        // Inputs cost disk and CPU; only now that a peer looks better (unless
        // the compile cache collected them already).
        let Prepared { spec, files, .. } = match prepared {
            Some(r) => r?,
            None => self.prepare(spec, info, sensitive).await?,
        };

        // Second round: the proc-macro closure may have narrowed the peers, and
        // the upload is known now.
        let (views, any) = self.peer_views(&spec);
        if views.is_empty() {
            return Err(if any {
                LocalReason::PeersBusy
            } else {
                LocalReason::NoPeer
            });
        }
        let cands: Vec<Candidate> = {
            let peers = lock(&self.peers);
            views
                .iter()
                .map(|v| {
                    let sent = peers.get(&v.id).map(|p| &p.sent);
                    let upload: u64 = spec
                        .inputs
                        .iter()
                        .filter(|i| !sent.is_some_and(|s| s.contains(&i.hash)))
                        .map(|i| i.size)
                        .sum();
                    Candidate {
                        free_slots: v.free,
                        speed: v.speed,
                        upload_bytes: upload,
                        bandwidth_bps: v.bandwidth,
                    }
                })
                .collect()
        };
        let first = match scheduler::choose(local_free, self.cfg.prefer_remote, &cands) {
            Choice::Local => return Err(LocalReason::LocalSlotFree),
            Choice::Peer(i) => i,
        };
        let mut order: Vec<usize> = (0..views.len()).filter(|i| *i != first).collect();
        order.sort_by_key(|i| std::cmp::Reverse(cands[*i].score()));
        let targets: Vec<NodeId> = std::iter::once(first)
            .chain(order)
            .map(|i| views[i].id)
            .collect();

        if let Some(p) = lock(&self.peers).get_mut(&targets[0]) {
            p.pending += 1;
        }
        Ok(Plan {
            spec,
            targets: targets.clone(),
            files,
            pending: std::sync::Mutex::new(Some(PendingGuard {
                node: self.clone(),
                peer: targets[0],
            })),
        })
    }

    // --------------------------------------------------------------- remote

    /// Connect to `peer`: its addresses are tried at the same time and the
    /// first one that answers wins (CF-27): one dead address must not cost
    /// seconds per target. The winner is remembered and goes first next time.
    pub(crate) async fn connect_peer(
        self: &Arc<Self>,
        peer: NodeId,
        deadline: tokio::time::Instant,
    ) -> Option<Race<crate::transport::FramedConn<crate::transport::ClientStream>>> {
        let (mut addrs, good) = {
            let peers = lock(&self.peers);
            let p = peers.get(&peer)?;
            (p.info.addrs.clone(), p.good_addr.clone())
        };
        // The address that worked last time starts first; the others join
        // after a short head start, so a healthy peer sees one connection.
        let mut head_start = false;
        if let Some(g) = good
            && let Some(i) = addrs.iter().position(|a| *a == g)
        {
            addrs.swap(0, i);
            head_start = true;
        }
        let me = self.clone();
        Some(race_first(
            addrs,
            deadline,
            if head_start {
                ADDR_HEAD_START
            } else {
                Duration::ZERO
            },
            move |a: String| {
                let me = me.clone();
                async move {
                    let (pid, _hello, conn) =
                        match crate::transport::connect(&me.tls, &me.id, &a).await {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::info!("link to a peer address failed: {e:#}");
                                return None;
                            }
                        };
                    if pid != peer {
                        tracing::info!("link reached another node than the one asked for");
                        return None;
                    }
                    Some(conn)
                }
            },
        ))
    }

    /// Whether a direct connection to `peer` is worth trying now, and whether
    /// a dial-back can follow (a live control connection exists).
    fn direct_and_dial_back(&self, peer: NodeId) -> Option<(bool, bool)> {
        let now = Instant::now();
        let peers = lock(&self.peers);
        let p = peers.get(&peer)?;
        Some((
            p.direct_failed_until.is_none_or(|t| t <= now),
            !p.ctrl.is_empty(),
        ))
    }

    /// The deadline for direct attempts: with a dial-back still possible they
    /// get part of the time only, so the dial-back has time too.
    fn direct_deadline(
        deadline: tokio::time::Instant,
        can_dial_back: bool,
    ) -> tokio::time::Instant {
        if !can_dial_back {
            return deadline;
        }
        let now = tokio::time::Instant::now();
        let half = deadline.saturating_duration_since(now) / 2;
        deadline.min(now + DIRECT_BUDGET.min(half))
    }

    /// Remember how `peer` was (or was not) reached (CF-34).
    fn note_reach(&self, peer: NodeId, reach: Reach) {
        let now = Instant::now();
        let mut peers = lock(&self.peers);
        let Some(p) = peers.get_mut(&peer) else {
            return;
        };
        if p.reach != reach {
            tracing::info!(
                peer = crate::proto::log_safe(&p.info.node_name).as_str(),
                "job connections: {reach:?}"
            );
        }
        p.reach = reach;
        match reach {
            Reach::Direct => {
                p.direct_failed_until = None;
                p.unreachable_until = None;
            }
            Reach::DialBack => p.unreachable_until = None,
            Reach::Failing => p.unreachable_until = Some(now + UNREACHABLE_PAUSE),
            Reach::Untried => {}
        }
    }

    fn note_direct_failed(&self, peer: NodeId) {
        if let Some(p) = lock(&self.peers).get_mut(&peer) {
            p.direct_failed_until = Some(Instant::now() + DIRECT_RETRY_AFTER);
        }
    }

    /// Ask `peer` over its control connection to open a connection to this
    /// node (CF-34) and wait for it until `deadline`.
    pub(crate) async fn dial_back_link(
        self: &Arc<Self>,
        peer: NodeId,
        deadline: tokio::time::Instant,
    ) -> Option<Link> {
        let ctrl = lock(&self.peers).get(&peer)?.ctrl.first()?.1.clone();
        let token: [u8; 16] = rand::random();
        let (tx, rx) = tokio::sync::oneshot::channel();
        lock(&self.dialbacks).insert(token, PendingDial { peer, tx });
        let _cleanup = DialCleanup { node: self, token };
        let wait = deadline.min(tokio::time::Instant::now() + DIALBACK_WAIT);
        tokio::time::timeout_at(wait, ctrl.send(Message::DialBack { token }))
            .await
            .ok()?
            .ok()?;
        let link = tokio::time::timeout_at(wait, rx).await.ok()?.ok()?;
        self.count("dialbacks", 1);
        Some(link)
    }

    /// A connection to `peer` for one request, direct or dialed back, before
    /// `deadline` (the compile cache's downloads).
    pub(crate) async fn open_conn(
        self: &Arc<Self>,
        peer: NodeId,
        deadline: tokio::time::Instant,
    ) -> Option<Link> {
        let (direct_ok, can_dial_back) = self.direct_and_dial_back(peer)?;
        if direct_ok {
            let d = Self::direct_deadline(deadline, can_dial_back);
            if let Some(mut race) = self.connect_peer(peer, d).await
                && let Some((_, conn)) = race.next().await
            {
                return Some(Link::new(conn));
            }
            self.note_direct_failed(peer);
        }
        if !can_dial_back {
            return None;
        }
        self.dial_back_link(peer, deadline).await
    }

    /// Send `Reserve` on a fresh job connection and wait for the answer.
    async fn reserve(
        &self,
        mut link: Link,
        job_id: JobId,
        live: &Liveness,
        deadline: tokio::time::Instant,
    ) -> Reservation {
        if let Err(e) = link.send(Message::Reserve { job_id }).await {
            tracing::info!("could not send Reserve: {e:#}");
            return Reservation::Failed;
        }
        let wait = deadline.min(tokio::time::Instant::now() + RESERVE_WAIT);
        match tokio::time::timeout_at(wait, link.recv()).await {
            Ok(Some(Message::Reserved { .. })) => {
                // Real progress: a frame from the peer.
                live.touch();
                Reservation::Ok(link)
            }
            Ok(Some(Message::Busy { .. })) => Reservation::Busy,
            _ => {
                tracing::info!("no Reserved from the peer (closed, too slow or another frame)");
                Reservation::Failed
            }
        }
    }

    /// Open the job connection to `peer` and reserve a slot, all before
    /// `deadline`. Direct first (its addresses race, CF-27); when no address
    /// answers, the peer is asked over the control connection to dial back
    /// (CF-34), and for [`DIRECT_RETRY_AFTER`] that comes first.
    async fn open_job_link(
        self: &Arc<Self>,
        peer: NodeId,
        job_id: JobId,
        live: &Liveness,
        deadline: tokio::time::Instant,
    ) -> Option<Link> {
        let (direct_ok, can_dial_back) = self.direct_and_dial_back(peer)?;
        if direct_ok {
            let d = Self::direct_deadline(deadline, can_dial_back);
            let mut answered = false;
            if let Some(mut race) = self.connect_peer(peer, d).await {
                while let Some((addr, conn)) = race.next().await {
                    answered = true;
                    match self.reserve(Link::new(conn), job_id, live, deadline).await {
                        Reservation::Ok(link) => {
                            if let Some(p) = lock(&self.peers).get_mut(&peer) {
                                p.good_addr = Some(addr);
                            }
                            self.note_reach(peer, Reach::Direct);
                            return Some(link);
                        }
                        Reservation::Busy => return None,
                        Reservation::Failed => continue,
                    }
                }
            }
            // The peer answered but did not take the job: a connection the
            // other way round would not change that.
            if answered {
                return None;
            }
            self.note_direct_failed(peer);
        }
        let link = if can_dial_back {
            self.dial_back_link(peer, deadline).await
        } else {
            None
        };
        let Some(link) = link else {
            self.note_reach(peer, Reach::Failing);
            return None;
        };
        match self.reserve(link, job_id, live, deadline).await {
            Reservation::Ok(link) => {
                self.note_reach(peer, Reach::DialBack);
                Some(link)
            }
            _ => None,
        }
    }

    /// Ask the peers whether one of them holds the cache entry for `key`
    /// (DESIGN section 16): `CacheQuery` goes over each online peer's existing
    /// authenticated control connection (no new connection per question, so
    /// questions cannot use up the unauthenticated accept slots job connects
    /// need), at most [`crate::node::CACHE_ASKS_PER_PEER`] in flight to one
    /// peer, all within [`CACHE_ASK_DEADLINE`], at most [`CACHE_ASK_PEERS`]
    /// peers and [`CACHE_ASK_CONCURRENCY`] lookups in flight on this node. A
    /// peer that did not answer in time is not asked again for
    /// [`CACHE_SLOW_PAUSE`]. The first `CacheHave` wins; only then is a
    /// connection opened (at most [`CACHE_FETCH_CONCURRENCY`] at once) and
    /// returned, ready for `CacheFetch`.
    pub(crate) async fn farm_lookup(self: &Arc<Self>, key: &CacheKey) -> Option<Link> {
        let now = Instant::now();
        let peers: Vec<NodeId> = {
            let book = lock(&self.peers);
            let mut ids: Vec<NodeId> = book
                .iter()
                .filter(|(_, p)| p.conns > 0 && p.status.is_some() && !p.ctrl.is_empty())
                .filter(|(_, p)| p.cache_slow_until.is_none_or(|t| t <= now))
                .map(|(id, _)| *id)
                .collect();
            ids.sort();
            ids.truncate(CACHE_ASK_PEERS);
            ids
        };
        if peers.is_empty() {
            return None;
        }
        // No permit: the node is already asking a lot; this job is a miss.
        let permit = self.cache_ask_sem.clone().try_acquire_owned().ok()?;
        let deadline = tokio::time::Instant::now() + CACHE_ASK_DEADLINE;
        let mut set = tokio::task::JoinSet::new();
        for peer in &peers {
            let (me, peer, key) = (self.clone(), *peer, key.0);
            set.spawn(async move { (peer, me.cache_ask(peer, key, deadline).await) });
        }
        self.count("cache_farm_asks", peers.len() as u64);
        let mut answered: HashSet<NodeId> = HashSet::new();
        let mut holder: Option<NodeId> = None;
        while let Ok(Some(joined)) = tokio::time::timeout_at(deadline, set.join_next()).await {
            if let Ok((peer, Some(have))) = joined {
                answered.insert(peer);
                if have.is_some() {
                    holder = Some(peer);
                    break;
                }
            }
        }
        drop(set);
        drop(permit);
        if holder.is_none() {
            let mut book = lock(&self.peers);
            for peer in peers.iter().filter(|p| !answered.contains(p)) {
                if let Some(p) = book.get_mut(peer) {
                    p.cache_slow_until = Some(Instant::now() + CACHE_SLOW_PAUSE);
                }
            }
        }
        let holder = holder?;
        // The download needs its own connection. A hit saves a compile, so it
        // may wait a moment for a place, but never for long.
        let deadline = tokio::time::Instant::now() + CACHE_FETCH_CONNECT;
        let _fetch =
            tokio::time::timeout_at(deadline, self.cache_fetch_sem.clone().acquire_owned())
                .await
                .ok()?
                .ok()?;
        self.open_conn(holder, deadline).await
    }

    async fn run_remote(
        self: &Arc<Self>,
        wrapper: &mut Link,
        plan: &Plan,
        info: &SubmitInfo,
        record: bool,
        live: &Liveness,
    ) -> RemoteEnd {
        let _running = RunningGuard::new(self, true);
        let _remote = RemoteScope::new(live);
        // One deadline for the whole open phase, however many targets and
        // addresses: cargo waits for this, so it must be short.
        let deadline = tokio::time::Instant::now() + OPEN_DEADLINE;
        for target in plan.targets.iter().take(2) {
            let Some(link) = self
                .open_job_link(*target, plan.spec.job_id, live, deadline)
                .await
            else {
                continue;
            };
            lock(&plan.pending).take();
            let started = Instant::now();
            let end = self
                .drive_remote(
                    wrapper,
                    link,
                    &plan.spec,
                    info,
                    Source::Job {
                        plan,
                        target: *target,
                    },
                    // Only a job the cache may hold (`ctx` exists): a
                    // sensitive or local-only job leaves nothing in the store.
                    record,
                    live,
                )
                .await;
            if matches!(end, RemoteEnd::Done(_)) {
                tracing::debug!(
                    crate_name = plan.spec.crate_name.as_str(),
                    ms = started.elapsed().as_millis() as u64,
                    "remote job done"
                );
            }
            return end;
        }
        RemoteEnd::Fallback("no peer accepted the job".into())
    }

    /// Run the job on one open job connection (or replay a cache entry from
    /// one) and apply what comes back. `record`: keep the result for the
    /// compile cache.
    #[allow(clippy::too_many_arguments)]
    async fn drive_remote(
        self: &Arc<Self>,
        wrapper: &mut Link,
        mut link: Link,
        spec: &JobSpec,
        info: &SubmitInfo,
        source: Source<'_>,
        record: bool,
        live: &Liveness,
    ) -> RemoteEnd {
        match self
            .drive_remote_inner(wrapper, &mut link, spec, info, source, record, live)
            .await
        {
            Ok(end) => {
                link.close().await;
                end
            }
            Err(e) => {
                let _ = link.send(Message::Cancel).await;
                link.close().await;
                RemoteEnd::Fallback(format!("{e:#}"))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn drive_remote_inner(
        self: &Arc<Self>,
        wrapper: &mut Link,
        link: &mut Link,
        spec: &JobSpec,
        info: &SubmitInfo,
        source: Source<'_>,
        record: bool,
        live: &Liveness,
    ) -> anyhow::Result<RemoteEnd> {
        match &source {
            Source::Job { .. } => {
                link.send(Message::JobRequest {
                    spec: spec.clone(),
                    blob_key: self.id.blob_key,
                })
                .await?
            }
            Source::Cache(Origin::Farm) => {
                let key = ccache::cache_key(spec);
                link.send(Message::CacheFetch { key: key.0 }).await?;
            }
            Source::Cache(Origin::Local) => {}
        }
        let mut capture = Captured {
            outputs: Vec::new(),
            lines: Vec::new(),
            dep_info: None,
        };
        // A result that cannot be recorded in full is not recorded at all.
        let mut record = record;
        let mut line_bytes = 0usize;
        let mut applied = 0u64;
        // One deadline for the whole attempt: a peer that trickles stderr lines
        // is "alive" to the idle limit but cannot keep the job past this.
        let job_deadline =
            Instant::now() + Duration::from_secs(self.cfg.job_timeout_min.max(1) * 60) + JOB_SLACK;
        let mut pending: Option<(Assembler, ClientPath, BlobHash, u64)> = None;
        // Outputs are applied at `Exit`, except clean metadata and dep-info
        // files, which are applied at their notification line so cargo can
        // pipeline (CF-16). A tainted job leaves no other file behind.
        let mut relay = Relay::default();
        let mut budget = OutputBudget::default();
        // Started with the job: all `NeedBlobs` rounds share it.
        let upload_deadline = Instant::now() + UPLOAD_DEADLINE;
        let style = spec.out_dir.style;
        // The first `error` diagnostic of the job, for the log when it exits
        // nonzero: a remote failure that a local run then cures needs a cause.
        let mut first_error: Option<String> = None;
        loop {
            let m = next_frame(link, job_deadline).await?;
            live.touch();
            match m {
                Message::NeedBlobs { hashes } => {
                    let Source::Job { plan, target } = &source else {
                        bail!("a cache entry asked for blobs");
                    };
                    // The upload has its own deadline; its chunks count as progress.
                    self.upload_blobs(link, plan, *target, &hashes, upload_deadline, live)
                        .await?
                }
                // The executor's heartbeat: the job is alive, nothing to apply.
                Message::Started | Message::Keepalive => {}
                Message::Stderr { line } => {
                    if pending.is_some() {
                        bail!("stderr inside an output transfer");
                    }
                    // An early notice moves a staged file into the client's
                    // directory: only for a wrapper that is still waiting.
                    if crate::rewrite::artifact(&line).is_some() && self.wrapper_gone(wrapper).await
                    {
                        let _ = link.send(Message::Cancel).await;
                        return Ok(RemoteEnd::WrapperGone);
                    }
                    if first_error.is_none() {
                        first_error = first_error_message(&line);
                    }
                    if record {
                        line_bytes += line.len() + 1;
                        if line_bytes > ccache::MAX_LINES_BYTES {
                            record = false;
                        } else {
                            capture.lines.push((false, line.clone()));
                        }
                    }
                    if let Some((m, early)) = relay.offer(Message::Stderr { line }, style)? {
                        if early {
                            self.count("early_notices", 1);
                        }
                        if wrapper.send(m).await.is_err() {
                            let _ = link.send(Message::Cancel).await;
                            return Ok(RemoteEnd::Done(DoneInfo::default()));
                        }
                    }
                }
                Message::Stdout { line } => {
                    if pending.is_some() {
                        bail!("stdout inside an output transfer");
                    }
                    if record {
                        line_bytes += line.len() + 1;
                        if line_bytes > ccache::MAX_LINES_BYTES {
                            record = false;
                        } else {
                            capture.lines.push((true, line.clone()));
                        }
                    }
                    if let Some((m, _)) = relay.offer(Message::Stdout { line }, style)?
                        && wrapper.send(m).await.is_err()
                    {
                        let _ = link.send(Message::Cancel).await;
                        return Ok(RemoteEnd::Done(DoneInfo::default()));
                    }
                }
                Message::Output { path, hash, size } => {
                    if pending.is_some() {
                        bail!("output started inside an output transfer");
                    }
                    if !output_allowed(&spec.out_dir, &info.produces, &path) {
                        bail!("peer sent an output outside the job's out-dir");
                    }
                    if size > inputs::MAX_TOTAL {
                        bail!("output too large");
                    }
                    budget.admit(size)?;
                    pending = Some((Assembler::default(), path, hash, size));
                }
                Message::BlobChunk {
                    hash,
                    offset,
                    last,
                    data,
                } => {
                    let Some((asm, path, want, size)) = pending.as_mut() else {
                        bail!("blob chunk without an output")
                    };
                    if hash != *want || offset > *size {
                        bail!("unexpected blob chunk");
                    }
                    if let Some(plain) = asm.push(&hash, offset, last, &data)? {
                        if plain.len() as u64 != *size {
                            bail!("output size differs from its announcement");
                        }
                        let dest = path.to_local();
                        let n = plain.len() as u64;
                        let to = dest.clone();
                        let node = self.clone();
                        let hash = *want;
                        let keep = record;
                        let (tmp, kept, dep) = tokio::task::spawn_blocking(move || {
                            let tmp = write_temp(&to, &plain)?;
                            // The compile cache keeps what it can verify later.
                            let kept = keep
                                && node
                                    .ccache
                                    .as_ref()
                                    .is_some_and(|c| c.put_blob(&hash, &plain).is_ok());
                            let dep = (keep && to.extension().is_some_and(|e| e == "d"))
                                .then(|| String::from_utf8(plain).ok())
                                .flatten();
                            anyhow::Ok((tmp, kept, dep))
                        })
                        .await??;
                        relay.staged.add(path.clone(), tmp, dest);
                        if !matches!(source, Source::Cache(Origin::Local)) {
                            self.count("bytes_down", n);
                        }
                        applied += n;
                        if record {
                            if kept {
                                capture.outputs.push(EntryOutput {
                                    path: path.clone(),
                                    hash,
                                    size: n,
                                });
                                if dep.is_some() {
                                    capture.dep_info = dep;
                                }
                            } else {
                                record = false;
                            }
                        }
                        pending = None;
                    }
                }
                Message::Tainted { .. } => {
                    // The peer's sandbox path is inside an output: use none of them.
                    relay.taint();
                    let _ = link.send(Message::Cancel).await;
                    return Ok(RemoteEnd::Tainted);
                }
                Message::Exit { code } => {
                    if pending.is_some() {
                        bail!("exit inside an output transfer");
                    }
                    // Files first, then the lines that announce them, then `Exit`.
                    if self.wrapper_gone(wrapper).await {
                        let _ = link.send(Message::Cancel).await;
                        return Ok(RemoteEnd::WrapperGone);
                    }
                    let ok = code == 0;
                    if !ok {
                        // Info: only the crate name and the first error message
                        // (peer text, escaped and cut), never more of the output.
                        tracing::info!(
                            crate_name = spec.crate_name.as_str(),
                            code,
                            "remote compile failed: {}",
                            crate::proto::log_safe(
                                first_error.as_deref().unwrap_or("no error line")
                            )
                        );
                    }
                    let held = {
                        let mut r = std::mem::take(&mut relay);
                        tokio::task::spawn_blocking(move || r.finish(ok)).await??
                    };
                    lock(&self.depgraph).finish(&info.produces, code == 0);
                    if let Source::Job { .. } = &source {
                        self.count(
                            if code == 0 {
                                "remote_ok"
                            } else {
                                "remote_exit_nonzero"
                            },
                            1,
                        );
                    }
                    for m in held {
                        if wrapper.send(m).await.is_err() {
                            let _ = link.send(Message::Cancel).await;
                            return Ok(RemoteEnd::Done(DoneInfo::default()));
                        }
                    }
                    if let Source::Job { target, .. } = &source
                        && !spec.no_retain
                    {
                        let mut peers = lock(&self.peers);
                        if let Some(p) = peers.get_mut(target) {
                            if p.sent.len() > crate::node::MAX_SENT_HASHES {
                                p.sent.clear();
                            }
                            p.sent.extend(spec.inputs.iter().map(|i| i.hash));
                        }
                    }
                    wrapper.send(Message::Exit { code }).await.ok();
                    // The wrapper has its answer; it speaks again only to ask
                    // for a local retry, and `handle_submit` resumes the
                    // keepalives then.
                    live.stopped.store(true, Ordering::Release);
                    return Ok(RemoteEnd::Done(DoneInfo {
                        capture: (record && code == 0).then_some(capture),
                        bytes: applied,
                    }));
                }
                Message::Error { message } => {
                    bail!("peer reported: {}", crate::proto::log_safe(&message))
                }
                _ => bail!("unexpected message from the peer"),
            }
        }
    }

    /// The wrapper that submitted this job went away (it ran rustc itself after
    /// a silence, or died), or the node is shutting down. Checked immediately
    /// before anything is written to the client's directories: a node that was
    /// only stalled and resumes must not rename remote outputs onto files the
    /// wrapper's local rustc is writing (CF-25).
    async fn wrapper_gone(&self, wrapper: &mut Link) -> bool {
        self.shutdown.is_cancelled() || wrapper.peer_gone(WRAPPER_CHECK).await
    }

    async fn upload_blobs(
        self: &Arc<Self>,
        link: &Link,
        plan: &Plan,
        target: NodeId,
        hashes: &[BlobHash],
        deadline: Instant,
        live: &Liveness,
    ) -> anyhow::Result<()> {
        let started = Instant::now();
        let mut wire = 0u64;
        for h in hashes {
            let path = plan
                .files
                .get(h)
                .ok_or_else(|| anyhow::anyhow!("peer asked for a blob this job does not have"))?
                .clone();
            let data = tokio::task::spawn_blocking(move || std::fs::read(path)).await??;
            // The file may have changed since it was hashed; the build is then
            // not what cargo asked for, so do not ship it.
            if BlobHash::of(&data) != *h {
                bail!("an input file changed while the job was prepared");
            }
            for (offset, last, chunk) in cas::chunks(&data) {
                wire += chunk.len() as u64;
                send_by(
                    link,
                    Message::BlobChunk {
                        hash: *h,
                        offset,
                        last,
                        data: chunk,
                    },
                    deadline,
                )
                .await?;
                live.touch();
            }
        }
        self.count("bytes_up", wire);
        if wire >= MIN_MEASURED_UPLOAD {
            let secs = started.elapsed().as_secs_f64().max(0.001);
            let bps = ((wire as f64 * 8.0) / secs) as u64;
            if let Some(p) = lock(&self.peers).get_mut(&target) {
                let blended = (p.bandwidth_bps as f64 * 0.7 + bps as f64 * 0.3) as u64;
                p.bandwidth_bps = blended.clamp(1_000_000, 100_000_000_000);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    /// CF-29: the cause of a remote failure is the first `error` diagnostic.
    #[test]
    fn first_error_message_picks_errors_only() {
        let warn = r#"{"level":"warning","message":"unused variable"}"#;
        let err = r#"{"level":"error","message":"couldn't read `x.rs`: path too long"}"#;
        assert_eq!(super::first_error_message(warn), None);
        assert_eq!(
            super::first_error_message(err).as_deref(),
            Some("couldn't read `x.rs`: path too long")
        );
        assert_eq!(
            super::first_error_message("error: linking failed").as_deref(),
            Some("error: linking failed")
        );
        assert_eq!(super::first_error_message("note: hi"), None);
        // An artifact notice has no level.
        assert_eq!(
            super::first_error_message(r#"{"artifact":"a","emit":"link"}"#),
            None
        );
    }

    /// CF-28: a local run that happened because the crate failed remotely or
    /// the wrapper rejected the remote result is not cached; the ordinary
    /// reasons are.
    #[test]
    fn local_results_of_crates_with_hidden_inputs_are_not_stored() {
        use LocalReason::*;
        for r in [RemoteCompileErrorRetry, EnvDependency] {
            assert!(!stores_local_result(r), "{r:?}");
        }
        for r in [
            NoPeer,
            PeersBusy,
            LocalSlotFree,
            RemoteFailed,
            SandboxPathInOutput,
            NotEligible,
        ] {
            assert!(stores_local_result(r), "{r:?}");
        }
    }

    /// CF-27 F4: one dead address never costs more than the deadline, and a
    /// live address wins at once even when others hang.
    #[tokio::test]
    async fn the_first_connect_to_answer_wins_and_the_rest_are_cancelled() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut race = race_first(
            vec!["dead1".into(), "live".into(), "dead2".into()],
            deadline,
            Duration::ZERO,
            |a: String| async move {
                if a == "live" {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Some(7u32)
                } else {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    None
                }
            },
        );
        let started = Instant::now();
        let (addr, v) = race.next().await.unwrap();
        assert_eq!((addr.as_str(), v), ("live", 7));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn all_addresses_dead_ends_at_the_deadline_not_per_address() {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        let addrs: Vec<String> = (0..16).map(|i| format!("a{i}")).collect();
        let mut race = race_first(addrs, deadline, Duration::ZERO, |_a: String| async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            None::<u32>
        });
        let started = Instant::now();
        assert!(race.next().await.is_none());
        // Sequentially this would be 16 attempts; here it is one deadline.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn failed_addresses_are_skipped_and_the_head_start_favours_the_first() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut race = race_first(
            vec!["good".into(), "other".into()],
            deadline,
            Duration::from_millis(200),
            |a: String| async move { (a == "good" || a == "other").then_some(1u8) },
        );
        let (first, _) = race.next().await.unwrap();
        assert_eq!(first, "good");
        let (second, _) = race.next().await.unwrap();
        assert_eq!(second, "other");
        assert!(race.next().await.is_none());
    }

    use super::*;
    use crate::proto::PathStyle;

    fn win(s: &str) -> ClientPath {
        ClientPath::parse(PathStyle::Windows, s).unwrap()
    }

    fn posix(s: &str) -> ClientPath {
        ClientPath::parse(PathStyle::Posix, s).unwrap()
    }

    #[test]
    fn output_inside_out_dir_is_allowed() {
        let out = win(r"C:\proj\target\release\deps");
        assert!(output_allowed(
            &out,
            &[],
            &win(r"C:\proj\target\release\deps\libx-1.rlib")
        ));
        assert!(output_allowed(
            &out,
            &[],
            &win(r"C:\proj\target\release\deps\sub\x.d")
        ));
        // Windows paths compare case-insensitively.
        assert!(output_allowed(
            &out,
            &[],
            &win(r"c:\PROJ\target\release\deps\x.rmeta")
        ));
    }

    #[test]
    fn output_outside_out_dir_is_refused() {
        let out = win(r"C:\proj\target\release\deps");
        assert!(!output_allowed(
            &out,
            &[],
            &win(r"C:\proj\target\release\deps2\x.rlib")
        ));
        assert!(!output_allowed(&out, &[], &win(r"C:\proj\src\lib.rs")));
        assert!(!output_allowed(
            &out,
            &[],
            &win(r"D:\proj\target\release\deps\x.rlib")
        ));
        assert!(!output_allowed(&out, &[], &out));
    }

    #[test]
    fn hand_built_paths_with_dot_dot_are_refused() {
        let out = win(r"C:\proj\target\deps");
        let evil = ClientPath {
            style: PathStyle::Windows,
            path: r"C:\proj\target\deps\..\..\src\lib.rs".into(),
        };
        assert!(!output_allowed(&out, &[], &evil));
        let rel = ClientPath {
            style: PathStyle::Windows,
            path: r"deps\x.rlib".into(),
        };
        assert!(!output_allowed(&out, &[], &rel));
    }

    #[test]
    fn produced_files_outside_out_dir_are_allowed_exactly() {
        let out = posix("/p/target/deps");
        let extra = posix("/p/other/x.rlib");
        assert!(output_allowed(&out, std::slice::from_ref(&extra), &extra));
        assert!(!output_allowed(
            &out,
            std::slice::from_ref(&extra),
            &posix("/p/other/y.rlib")
        ));
    }

    #[test]
    fn path_style_mismatch_is_refused() {
        let out = posix("/p/target/deps");
        assert!(!output_allowed(&out, &[], &win(r"C:\p\target\deps\x.rlib")));
    }

    #[test]
    fn prefix_matching_is_component_wise() {
        let prefixes = vec![r"C:\secret".to_string(), "not a path".to_string()];
        assert!(under_prefix(&prefixes, &win(r"C:\secret\proj\src\lib.rs")));
        assert!(!under_prefix(&prefixes, &win(r"C:\secrets\proj")));
        assert!(!under_prefix(&prefixes, &win(r"C:\other")));
    }

    #[test]
    fn staged_outputs_appear_only_on_commit_and_vanish_when_discarded() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (
            d.path().join("deps").join("liba.rlib"),
            d.path().join("deps").join("liba.rmeta"),
        );
        let names = |dir: &std::path::Path| -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };
        let mut st = Staged::default();
        st.add(posix("/a"), write_temp(&a, b"rlib").unwrap(), a.clone());
        st.add(posix("/b"), write_temp(&b, b"rmeta").unwrap(), b.clone());
        assert!(
            !a.exists() && !b.exists(),
            "staged files must not be at their destination"
        );
        // A tainted job: everything staged is deleted, nothing is written.
        st.discard();
        assert!(
            names(&d.path().join("deps")).is_empty(),
            "{:?}",
            names(&d.path().join("deps"))
        );
        // Dropping staged files (error path) cleans up as well.
        {
            let mut lost = Staged::default();
            lost.add(posix("/a"), write_temp(&a, b"x").unwrap(), a.clone());
        }
        assert!(names(&d.path().join("deps")).is_empty());
        // A good job: both files are moved into place.
        let mut ok = Staged::default();
        ok.add(posix("/a"), write_temp(&a, b"rlib").unwrap(), a.clone());
        ok.add(posix("/b"), write_temp(&b, b"rmeta").unwrap(), b.clone());
        ok.commit().unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), b"rlib");
        assert_eq!(
            names(&d.path().join("deps")),
            vec!["liba.rlib", "liba.rmeta"]
        );
    }

    fn notice(path: &str, emit: &str) -> Message {
        let line = serde_json::json!({ "artifact": path, "emit": emit }).to_string();
        Message::Stderr { line }
    }

    fn stage(r: &mut Relay, dir: &std::path::Path, name: &str, data: &[u8]) -> (PathBuf, String) {
        let dest = dir.join(name);
        let client = ClientPath::from_local(&dest, dir).expect("client path");
        let text = client.path.clone();
        r.staged
            .add(client, write_temp(&dest, data).unwrap(), dest.clone());
        (dest, text)
    }

    fn style_of(dir: &std::path::Path) -> crate::proto::PathStyle {
        ClientPath::from_local(dir, dir).expect("client path").style
    }

    /// CF-16: the metadata is in place and its notice goes out while the job
    /// is still running; the link artifact and everything after it waits for
    /// `Exit`, files first.
    #[test]
    fn clean_metadata_is_relayed_before_exit_and_the_rest_waits() {
        let d = tempfile::tempdir().unwrap();
        let style = style_of(d.path());
        let mut r = Relay::default();
        let (meta, meta_c) = stage(&mut r, d.path(), "libx.rmeta", b"meta");
        let (rlib, rlib_c) = stage(&mut r, d.path(), "libx.rlib", b"rlib");
        assert!(!meta.exists());

        let out = r
            .offer(notice(&meta_c, "metadata"), style)
            .unwrap()
            .expect("metadata notice goes out at once");
        assert!(out.1, "counted as an early notice");
        assert_eq!(std::fs::read(&meta).unwrap(), b"meta");
        assert!(!rlib.exists(), "the link output is still staged");

        // A diagnostic after the metadata keeps its place in line.
        let w = Message::Stderr {
            line: "warning: x".into(),
        };
        assert!(r.offer(w, style).unwrap().is_some());

        // The link notice is not early: it and later frames are held.
        assert!(r.offer(notice(&rlib_c, "link"), style).unwrap().is_none());
        let late = Message::Stderr {
            line: "warning: y".into(),
        };
        assert!(r.offer(late, style).unwrap().is_none());
        assert!(!rlib.exists());

        let held = r.finish(true).unwrap();
        assert_eq!(held.len(), 2);
        assert_eq!(std::fs::read(&rlib).unwrap(), b"rlib");
    }

    /// A notice whose file is not staged is never relayed early: cargo must
    /// not see a notice before its file exists.
    #[test]
    fn a_notice_without_its_staged_file_is_held() {
        let d = tempfile::tempdir().unwrap();
        let style = style_of(d.path());
        let mut r = Relay::default();
        let ghost = ClientPath::from_local(&d.path().join("libghost.rmeta"), d.path()).unwrap();
        assert!(
            r.offer(notice(&ghost.path, "metadata"), style)
                .unwrap()
                .is_none()
        );
        assert_eq!(r.held.len(), 1);
    }

    /// CF-25: a notice a peer repeats is dropped, early or held, so cargo never
    /// releases the dependents of a crate twice.
    #[test]
    fn a_repeated_artifact_notice_is_dropped() {
        let d = tempfile::tempdir().unwrap();
        let style = style_of(d.path());
        let mut r = Relay::default();
        let (_meta, meta_c) = stage(&mut r, d.path(), "libz.rmeta", b"meta");
        let (_rlib, rlib_c) = stage(&mut r, d.path(), "libz.rlib", b"rlib");
        assert!(
            r.offer(notice(&meta_c, "metadata"), style)
                .unwrap()
                .is_some()
        );
        assert!(
            r.offer(notice(&meta_c, "metadata"), style)
                .unwrap()
                .is_none(),
            "the second metadata notice must not reach the wrapper"
        );
        assert!(r.held.is_empty(), "and it is not held for later either");
        // A different kind for the same file is a different notice.
        assert!(r.offer(notice(&rlib_c, "link"), style).unwrap().is_none());
        assert_eq!(r.held.len(), 1);
        assert!(r.offer(notice(&rlib_c, "link"), style).unwrap().is_none());
        assert_eq!(r.held.len(), 1, "a repeated held notice is dropped too");
    }

    /// CF-25: a notice whose keys use a JSON unicode escape is still a notice:
    /// its file is staged and checked like any other, and it is not passed
    /// through as ordinary stderr.
    #[test]
    fn an_escaped_artifact_key_is_still_a_notice() {
        let d = tempfile::tempdir().unwrap();
        let style = style_of(d.path());
        let mut r = Relay::default();
        let (meta, meta_c) = stage(&mut r, d.path(), "libq.rmeta", b"meta");
        let bs = char::from(92u8);
        let line = serde_json::json!({ "emit": "metadata", "artifact": meta_c }).to_string();
        let line = line.replace("\"artifact\"", &format!("\"{bs}u0061rtifact\""));
        assert!(line.contains("u0061rtifact"));
        let out = r
            .offer(Message::Stderr { line }, style)
            .unwrap()
            .expect("an early notice for a staged file goes out");
        assert!(out.1, "it was handled as an artifact notice");
        assert_eq!(std::fs::read(&meta).unwrap(), b"meta");
    }

    /// CF-10 with CF-16: after a taint nothing staged is written and nothing
    /// held is relayed; only what was committed early stays.
    #[test]
    fn taint_after_an_early_metadata_keeps_only_the_clean_early_file() {
        let d = tempfile::tempdir().unwrap();
        let style = style_of(d.path());
        let mut r = Relay::default();
        let (meta, meta_c) = stage(&mut r, d.path(), "liby.rmeta", b"meta");
        let (rlib, rlib_c) = stage(&mut r, d.path(), "liby.rlib", b"tainted");
        assert!(
            r.offer(notice(&meta_c, "metadata"), style)
                .unwrap()
                .is_some()
        );
        assert!(r.offer(notice(&rlib_c, "link"), style).unwrap().is_none());
        r.taint();
        assert!(meta.exists());
        assert!(!rlib.exists());
        assert!(r.held.is_empty());
        let names: Vec<_> = std::fs::read_dir(d.path()).unwrap().collect();
        assert_eq!(names.len(), 1, "no temporary file is left");
    }

    /// CF-19: a job stages at most `MAX_OUTPUT_FILES` files and
    /// `MAX_OUTPUT_BYTES` bytes; the first output over a limit ends the job
    /// (which then reruns locally and leaves nothing staged behind).
    #[test]
    fn staged_outputs_are_capped_by_count_and_size() {
        let mut b = OutputBudget::default();
        for _ in 0..MAX_OUTPUT_FILES {
            b.admit(1).unwrap();
        }
        assert!(b.admit(0).is_err(), "file number {}", MAX_OUTPUT_FILES + 1);

        let mut b = OutputBudget::default();
        b.admit(MAX_OUTPUT_BYTES - 10).unwrap();
        b.admit(10).unwrap();
        assert!(b.admit(1).is_err(), "one byte over the total");
        let mut b = OutputBudget::default();
        assert!(b.admit(u64::MAX).is_err(), "no overflow");
        assert!(b.admit(MAX_OUTPUT_BYTES + 1).is_err());
    }

    /// CF-19: an upload to a peer that does not read ends at the deadline
    /// instead of waiting for room in the queue for ever.
    #[tokio::test]
    async fn an_upload_to_a_peer_that_never_reads_hits_the_deadline() {
        use crate::transport::FramedConn;
        // The peer end is kept open and never read: the 64 KiB pipe fills up
        // and the queue behind it after a few frames.
        let (a, _peer) = tokio::io::duplex(64 * 1024);
        let link = Link::new(FramedConn::new(a));
        let deadline = Instant::now() + Duration::from_millis(300);
        let started = Instant::now();
        let mut result = Ok(());
        for _ in 0..1000 {
            let m = Message::BlobChunk {
                hash: BlobHash([0; 32]),
                offset: 0,
                last: false,
                data: vec![0u8; 256 * 1024],
            };
            result = send_by(&link, m, deadline).await;
            if result.is_err() {
                break;
            }
        }
        let err = result.expect_err("the upload must not wait for ever");
        assert!(format!("{err:#}").contains("deadline"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    fn duplex_link() -> (Link, crate::transport::FramedConn<tokio::io::DuplexStream>) {
        use crate::transport::FramedConn;
        let (a, b) = tokio::io::duplex(64 * 1024);
        (Link::new(FramedConn::new(a)), FramedConn::new(b))
    }

    /// CF-25: a peer that sends nothing is given up on after the idle limit,
    /// and a quiet but open connection is not mistaken for a closed one.
    #[tokio::test]
    async fn a_silent_peer_ends_the_job_at_the_idle_limit() {
        let (mut link, _peer) = duplex_link();
        let started = Instant::now();
        let err = next_frame_within(
            &mut link,
            Instant::now() + Duration::from_secs(60),
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("peer silent"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// CF-25: frames that keep coming (stderr trickled one line per beat) reset
    /// the idle limit but cannot extend the overall deadline.
    #[tokio::test]
    async fn a_peer_that_trickles_frames_cannot_outlive_the_job_deadline() {
        let (mut link, mut peer) = duplex_link();
        let feeder = tokio::spawn(async move {
            loop {
                if peer.send(&Message::Keepalive).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        let deadline = Instant::now() + Duration::from_millis(500);
        let result = loop {
            match next_frame_within(&mut link, deadline, Duration::from_millis(300)).await {
                Ok(_) => {}
                Err(e) => break e,
            }
        };
        feeder.abort();
        assert!(
            format!("{result:#}").contains("overall deadline"),
            "{result:#}"
        );
        assert!(Instant::now() < deadline + Duration::from_secs(2));
    }

    /// CF-25: the wrapper hears keepalives while the node waits for a peer slot
    /// or a local one, and while a peer is demonstrably alive; not once a peer
    /// went quiet, and not after the wrapper was told to run rustc itself.
    #[test]
    fn keepalive_to_the_wrapper_needs_a_live_job() {
        let live = Liveness::new();
        assert!(
            live.keepalive_due(),
            "no remote attempt: the node is just busy"
        );
        live.set_remote(true);
        assert!(live.keepalive_due(), "peer heard from just now");
        // Age the last frame by hand: 31 s ago.
        let aged = Liveness {
            base: Instant::now() - Duration::from_secs(40),
            last_ms: AtomicU64::new(9_000),
            remote: AtomicBool::new(true),
            stopped: AtomicBool::new(false),
        };
        assert!(!aged.keepalive_due(), "peer silent for 31 s");
        aged.touch();
        assert!(aged.keepalive_due(), "a frame arrived");
        aged.remote.store(false, Ordering::SeqCst);
        aged.stopped.store(true, Ordering::SeqCst);
        assert!(!aged.keepalive_due(), "the wrapper runs rustc itself");
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_files() {
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join("deps").join("a.rlib");
        write_atomic(&dest, b"one").unwrap();
        write_atomic(&dest, b"two").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1);
    }
}
