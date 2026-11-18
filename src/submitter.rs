//! Submitting side (DESIGN sections 4, 8, 10): serve the wrappers that talk to
//! this node over IPC. For each `Submit` the node decides where the job runs,
//! builds its inputs, runs it on a peer and applies the result, or hands the
//! wrapper a local slot.

use crate::cas::{self, Assembler};
use crate::depgraph::is_proc_macro_lib;
use crate::inputs::{self, CollectError};
use crate::link::Link;
use crate::node::{Node, lock};
use crate::proto::{
    BlobHash, ClientPath, EnvValue, InputFile, JobId, JobSpec, LocalReason, Message, NodeId,
};
use crate::scheduler::{self, Candidate, Choice, JobNeeds};
use anyhow::bail;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// After a remote `Exit` the wrapper may still ask for a local retry.
const RETRY_WAIT: Duration = Duration::from_secs(60);
const RESERVE_WAIT: Duration = Duration::from_secs(10);
/// Uploads smaller than this say nothing about the link speed.
const MIN_MEASURED_UPLOAD: u64 = 2 * 1024 * 1024;
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

enum RemoteEnd {
    /// The final `Exit` was relayed to the wrapper.
    Done,
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
        if self.holding {
            self.held.push(m);
            return Ok(None);
        }
        if let Message::Stderr { line } = &m
            && let Some((file, emit)) = crate::rewrite::artifact(line)
        {
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

struct Built {
    inputs: Vec<InputFile>,
    files: HashMap<BlobHash, PathBuf>,
    has_proc_macro: bool,
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

    let closure = lock(&node.depgraph).closure(&info.externs).map_err(|e| {
        tracing::debug!(crate_name = spec.crate_name.as_str(), "{e}");
        LocalReason::UnknownDependency
    })?;
    let has_proc_macro = closure.iter().any(is_proc_macro_lib);

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
    Ok(Built {
        inputs: inputs_out,
        files,
        has_proc_macro,
    })
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
                    let tx = link.tx.clone();
                    let ticker = tokio::spawn(async move {
                        let mut t = tokio::time::interval(crate::ipc::KEEPALIVE_INTERVAL);
                        t.tick().await;
                        loop {
                            t.tick().await;
                            if tx.send(Message::Keepalive).await.is_err() {
                                return;
                            }
                        }
                    });
                    self.handle_submit(&mut link, spec, info, force_local).await;
                    ticker.abort();
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
    ) {
        lock(&self.depgraph).begin(&info.produces, &info.externs);
        if force_local {
            // The wrapper knows a variable this crate reads is not forwarded
            // (CF-18): no peer is tried, no inputs are collected.
            self.local_phase(wrapper, LocalReason::EnvDependency, &info)
                .await;
            return;
        }
        let plan = match self.plan(spec, &info).await {
            Ok(p) => p,
            Err(reason) => {
                self.local_phase(wrapper, reason, &info).await;
                return;
            }
        };
        match self.run_remote(wrapper, &plan, &info).await {
            RemoteEnd::Done => {
                // The wrapper may reject the result (compile error of a
                // non-registry crate, env-dep mismatch) and ask for a slot.
                if let Ok(Some(Message::RetryLocal { reason })) =
                    tokio::time::timeout(RETRY_WAIT, wrapper.recv()).await
                {
                    self.local_phase(wrapper, reason, &info).await;
                }
            }
            RemoteEnd::Fallback(why) => {
                tracing::debug!(
                    crate_name = plan.spec.crate_name.as_str(),
                    "remote job failed: {why}"
                );
                self.count("remote_failed", 1);
                self.local_phase(wrapper, LocalReason::RemoteFailed, &info)
                    .await;
            }
            RemoteEnd::Tainted => {
                tracing::info!(
                    crate_name = plan.spec.crate_name.as_str(),
                    "remote output embeds the sandbox path, rerunning locally"
                );
                self.local_phase(wrapper, LocalReason::SandboxPathInOutput, &info)
                    .await;
            }
        }
    }

    /// Wait for a local slot (FIFO), hand it to the wrapper, and wait until its
    /// rustc is done.
    async fn local_phase(
        self: &Arc<Self>,
        wrapper: &mut Link,
        reason: LocalReason,
        info: &SubmitInfo,
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
        let ok = loop {
            match wrapper.recv().await {
                Some(Message::LocalDone { code, .. }) => break code == 0,
                Some(_) => continue,
                None => break false,
            }
        };
        lock(&self.depgraph).finish(&info.produces, ok);
        drop(permit);
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
        for (id, p) in lock(&self.peers).iter() {
            let Some(st) = &p.status else { continue };
            if p.conns == 0 || !scheduler::eligible(st, &needs) {
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

    async fn plan(
        self: &Arc<Self>,
        mut spec: JobSpec,
        info: &SubmitInfo,
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

        // Inputs cost disk and CPU; only now that a peer looks better.
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
            files: built.files,
            pending: std::sync::Mutex::new(Some(PendingGuard {
                node: self.clone(),
                peer: targets[0],
            })),
        })
    }

    // --------------------------------------------------------------- remote

    async fn open_job_link(self: &Arc<Self>, peer: NodeId, job_id: JobId) -> Option<Link> {
        let addrs = lock(&self.peers)
            .get(&peer)
            .map(|p| p.info.addrs.clone())
            .unwrap_or_default();
        for a in addrs {
            let Ok((pid, _hello, conn)) = crate::transport::connect(&self.tls, &self.id, &a).await
            else {
                continue;
            };
            if pid != peer {
                continue;
            }
            let mut link = Link::new(conn);
            if link.send(Message::Reserve { job_id }).await.is_err() {
                continue;
            }
            match tokio::time::timeout(RESERVE_WAIT, link.recv()).await {
                Ok(Some(Message::Reserved { .. })) => return Some(link),
                Ok(Some(Message::Busy { .. })) => return None,
                _ => continue,
            }
        }
        None
    }

    async fn run_remote(
        self: &Arc<Self>,
        wrapper: &Link,
        plan: &Plan,
        info: &SubmitInfo,
    ) -> RemoteEnd {
        let _running = RunningGuard::new(self, true);
        for target in plan.targets.iter().take(2) {
            let Some(link) = self.open_job_link(*target, plan.spec.job_id).await else {
                continue;
            };
            lock(&plan.pending).take();
            let started = Instant::now();
            let end = self.drive_remote(wrapper, link, plan, info, *target).await;
            if matches!(end, RemoteEnd::Done) {
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

    /// Run the job on one open job connection and apply what comes back.
    async fn drive_remote(
        self: &Arc<Self>,
        wrapper: &Link,
        mut link: Link,
        plan: &Plan,
        info: &SubmitInfo,
        target: NodeId,
    ) -> RemoteEnd {
        match self
            .drive_remote_inner(wrapper, &mut link, plan, info, target)
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

    async fn drive_remote_inner(
        self: &Arc<Self>,
        wrapper: &Link,
        link: &mut Link,
        plan: &Plan,
        info: &SubmitInfo,
        target: NodeId,
    ) -> anyhow::Result<RemoteEnd> {
        let spec = &plan.spec;
        link.send(Message::JobRequest {
            spec: spec.clone(),
            blob_key: self.id.blob_key,
        })
        .await?;
        let idle = Duration::from_secs(self.cfg.job_timeout_min.max(1) * 60 + 120);
        let mut pending: Option<(Assembler, ClientPath, BlobHash, u64)> = None;
        // Outputs are applied at `Exit`, except clean metadata and dep-info
        // files, which are applied at their notification line so cargo can
        // pipeline (CF-16). A tainted job leaves no other file behind.
        let mut relay = Relay::default();
        let mut budget = OutputBudget::default();
        let style = spec.out_dir.style;
        loop {
            let m = tokio::time::timeout(idle, link.recv())
                .await
                .map_err(|_| anyhow::anyhow!("peer silent"))?;
            let Some(m) = m else {
                bail!("connection to the peer lost")
            };
            match m {
                Message::NeedBlobs { hashes } => {
                    self.upload_blobs(link, plan, target, &hashes).await?
                }
                Message::Started => {}
                Message::Stderr { line } => {
                    if pending.is_some() {
                        bail!("stderr inside an output transfer");
                    }
                    if let Some((m, early)) = relay.offer(Message::Stderr { line }, style)? {
                        if early {
                            self.count("early_notices", 1);
                        }
                        if wrapper.send(m).await.is_err() {
                            let _ = link.send(Message::Cancel).await;
                            return Ok(RemoteEnd::Done);
                        }
                    }
                }
                Message::Stdout { line } => {
                    if pending.is_some() {
                        bail!("stdout inside an output transfer");
                    }
                    if let Some((m, _)) = relay.offer(Message::Stdout { line }, style)?
                        && wrapper.send(m).await.is_err()
                    {
                        let _ = link.send(Message::Cancel).await;
                        return Ok(RemoteEnd::Done);
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
                        let tmp =
                            tokio::task::spawn_blocking(move || write_temp(&to, &plain)).await??;
                        relay.staged.add(path.clone(), tmp, dest);
                        self.count("bytes_down", n);
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
                    let ok = code == 0;
                    let held = {
                        let mut r = std::mem::take(&mut relay);
                        tokio::task::spawn_blocking(move || r.finish(ok)).await??
                    };
                    lock(&self.depgraph).finish(&info.produces, code == 0);
                    self.count(
                        if code == 0 {
                            "remote_ok"
                        } else {
                            "remote_exit_nonzero"
                        },
                        1,
                    );
                    for m in held {
                        if wrapper.send(m).await.is_err() {
                            let _ = link.send(Message::Cancel).await;
                            return Ok(RemoteEnd::Done);
                        }
                    }
                    if !spec.no_retain {
                        let mut peers = lock(&self.peers);
                        if let Some(p) = peers.get_mut(&target) {
                            if p.sent.len() > crate::node::MAX_SENT_HASHES {
                                p.sent.clear();
                            }
                            p.sent.extend(spec.inputs.iter().map(|i| i.hash));
                        }
                    }
                    wrapper.send(Message::Exit { code }).await.ok();
                    return Ok(RemoteEnd::Done);
                }
                Message::Error { message } => {
                    bail!("peer reported: {}", crate::proto::log_safe(&message))
                }
                _ => bail!("unexpected message from the peer"),
            }
        }
    }

    async fn upload_blobs(
        self: &Arc<Self>,
        link: &Link,
        plan: &Plan,
        target: NodeId,
        hashes: &[BlobHash],
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
                link.send(Message::BlobChunk {
                    hash: *h,
                    offset,
                    last,
                    data: chunk,
                })
                .await?;
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
