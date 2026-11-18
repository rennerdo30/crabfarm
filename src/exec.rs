//! Package C. Run one job on the executing peer (DESIGN section 9).
//!
//! Transport-agnostic: the caller (node, package D) moves frames; this module
//! materializes the sandbox, runs rustc under the node jobserver, and emits
//! [`ExecEvent`]s in the order the submitter must apply them.

use crate::cas::BlobStore;
use crate::paths::SandboxMap;
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
    Output { path: ClientPath, hash: BlobHash, data: Vec<u8> },
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

fn job_hex(id: &JobId) -> String {
    id.0.iter().map(|b| format!("{b:02x}")).collect()
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
            "RUSTFLAGS" | "OUT_DIR" | "TARGET" | "HOST" | "PROFILE" | "NUM_JOBS" | "OPT_LEVEL" | "DEBUG" | "MAKEFLAGS" | "MFLAGS"
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
/// path (`..` components): every path has to be what `ClientPath::parse` yields.
fn validate_paths(spec: &JobSpec) -> anyhow::Result<()> {
    for p in spec_paths(spec) {
        if ClientPath::parse(p.style, &p.path).as_ref() != Some(p) {
            bail!("job contains a path that is not absolute and normalized");
        }
    }
    Ok(())
}

/// Deletes the job directory when dropped. The normal path calls
/// `remove_job_dir` explicitly (with retries); this covers a dropped future.
struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Remove the job directory, retrying briefly: on Windows a file stays locked
/// for a moment after its process exited (virus scanner, handle teardown).
async fn remove_job_dir(dir: &Path) {
    for attempt in 0..10 {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                if attempt == 9 {
                    tracing::warn!("could not delete job directory {}: {e}", dir.display());
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
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

/// Sends events and remembers what was already emitted as an output.
struct Emitter<'a> {
    ctx: &'a ExecContext<'a>,
    spec: &'a JobSpec,
    map: &'a SandboxMap,
    rewriter: &'a Rewriter,
    events: &'a mpsc::Sender<ExecEvent>,
    /// Outputs already sent (hash of what was sent).
    emitted: HashMap<ClientPath, BlobHash>,
}

impl Emitter<'_> {
    async fn send(&self, ev: ExecEvent) -> anyhow::Result<()> {
        // The receiver is gone: nobody wants this job any more.
        self.events.send(ev).await.map_err(|_| anyhow::Error::from(ExecAborted::Cancelled))
    }

    /// Send one produced file (rewriting `.d` files) and store it.
    async fn output(&mut self, path: ClientPath, mut data: Vec<u8>) -> anyhow::Result<()> {
        if path.path.ends_with(".d") {
            data = self.rewriter.dep_info(&String::from_utf8_lossy(&data)).into_bytes();
        }
        let hash = BlobHash::of(&data);
        if !self.spec.no_retain
            && let Err(e) = self.ctx.store.put(self.ctx.owner, &self.ctx.key, &hash, &data)
        {
            // The result is still delivered; only the later cache hit is lost.
            tracing::warn!("could not store output {hash:?}: {e:#}");
        }
        self.emitted.insert(path.clone(), hash);
        self.send(ExecEvent::Output { path, hash, data }).await
    }

    async fn stderr_line(&mut self, line: &str) -> anyhow::Result<()> {
        if let Some((file, _emit)) = rewrite::artifact(line) {
            // The submitter must have the file before cargo sees the notice.
            let sandbox_cwd = self.map.to_sandbox(&self.spec.cwd);
            let p = Path::new(&file);
            let abs = if p.is_absolute() { p.to_path_buf() } else { sandbox_cwd.join(p) };
            let client = self
                .map
                .from_sandbox(&abs, self.spec.cwd.style)
                .context("artifact notification points outside the sandbox")?;
            let data = std::fs::read(&abs).with_context(|| format!("read artifact {}", client.path))?;
            self.output(client, data).await?;
        }
        self.send(ExecEvent::Stderr(self.rewriter.stderr_line(line))).await
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
    validate_paths(spec)?;
    let job_dir = ctx.jobs_dir.join(job_hex(&spec.job_id));
    let _guard = DirGuard(job_dir.clone());
    // A directory left by a crashed run would leak old files into this job.
    let _ = std::fs::remove_dir_all(&job_dir);
    let map = SandboxMap::new(job_dir.join("root"));

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
    for input in &spec.inputs {
        let dst = map.to_sandbox(&input.path);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let data = ctx
            .store
            .get(ctx.owner, &ctx.key, &input.hash)
            .with_context(|| format!("input {}", input.path.path))?;
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
        let dir = map.to_sandbox(d);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    }

    // 2. Arguments: our remaps first, so the user's own remaps (later) win.
    let remaps = map.remaps(spec_paths(spec));
    let mut args: Vec<String> = Vec::new();
    for (from, to) in &remaps {
        args.push("--remap-path-prefix".into());
        args.push(format!("{from}={to}"));
    }
    args.extend(render_args(&spec.args, &|p| map.render(p)));

    let rewriter = Rewriter { pairs: remaps, client_windows: style == crate::proto::PathStyle::Windows };

    // 3. Environment and process.
    let mut cmd = std::process::Command::new(&toolchain.rustc);
    cmd.args(&args);
    cmd.current_dir(map.to_sandbox(&spec.cwd));
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
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().with_context(|| format!("spawn rustc for {}", spec.crate_name))?;
    let stdout = child.stdout.take().context("no stdout pipe")?;
    let stderr = child.stderr.take().context("no stderr pipe")?;
    let (tx, mut rx) = mpsc::channel::<Line>(64);
    let readers = [spawn_reader(stdout, tx.clone(), false), spawn_reader(stderr, tx, true)];

    let deadline = tokio::time::Instant::now() + ctx.timeout;
    let mut em = Emitter { ctx, spec, map, rewriter: &rewriter, events, emitted: HashMap::new() };

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
            // Kill only the child this call spawned.
            let _ = child.kill().await;
            for r in &readers {
                r.abort();
            }
            return Err(e);
        }
    };
    let code = status.code().unwrap_or(-1);

    // 4. Outputs. After a failed compile the out-dir only holds partial files
    // that cargo would ignore, so nothing is collected.
    if code == 0 {
        let out_root = map.to_sandbox(&spec.out_dir);
        let mut files: Vec<PathBuf> = walkdir::WalkDir::new(&out_root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .collect();
        files.sort();
        for f in files {
            let Some(client) = map.from_sandbox(&f, style) else { continue };
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
                rewriter.dep_info(&String::from_utf8_lossy(&data)).into_bytes()
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
        tc.list().into_iter().find(|t| t.id.same_compiler(&id)).expect("active toolchain")
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
        Fixture { root, jobs, store, js: NodeJobserver::new(2).unwrap(), tc, _tmp: tmp }
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
            inputs: vec![InputFile { path: cp(&lib), hash, size: data.len() as u64, executable: false }],
            dirs: vec![cp(&out)],
            out_dir: cp(&out),
            crate_name: "fixture".into(),
            no_retain,
            needs_host: false,
        }
    }

    fn ctx<'a>(f: &'a Fixture, timeout: Duration) -> ExecContext<'a> {
        ExecContext { jobs_dir: f.jobs.clone(), store: &f.store, jobserver: &f.js, owner: OWNER, key: KEY, timeout }
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
        assert!(matches!(evs.last(), Some(ExecEvent::Exit(0))), "events: {evs:?}");
        let names: Vec<String> = evs
            .iter()
            .filter_map(|e| match e {
                ExecEvent::Output { path, .. } => Some(path.path.rsplit(['\\', '/']).next().unwrap().to_string()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"libfixture-abc.rmeta".to_string()), "{names:?}");
        assert!(names.contains(&"libfixture-abc.rlib".to_string()), "{names:?}");
        assert!(names.contains(&"fixture-abc.d".to_string()), "{names:?}");
        // Each file exactly once: a file already sent with its final content is not repeated.
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate outputs: {names:?}");

        // The rmeta Output precedes the stderr line announcing it.
        let rmeta_out = evs
            .iter()
            .position(|e| matches!(e, ExecEvent::Output { path, .. } if path.path.ends_with(".rmeta")))
            .unwrap();
        let rmeta_note = evs
            .iter()
            .position(|e| matches!(e, ExecEvent::Stderr(l) if l.contains("\"artifact\"") && l.contains("rmeta")))
            .expect("rmeta artifact line");
        assert!(rmeta_out < rmeta_note, "Output must come before its notification");

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
                        assert!(v["spans"][0]["file_name"].as_str().unwrap().starts_with(&client_root), "{l}");
                    }
                }
                ExecEvent::Output { path, data, hash } if path.path.ends_with(".d") => {
                    let text = String::from_utf8(data.clone()).unwrap();
                    assert!(!text.contains(&sandbox_root), "dep-info leaked the sandbox: {text}");
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
        assert!(!f.jobs.join(job_hex(&spec.job_id)).exists(), "sandbox not deleted");
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
                ExecEvent::Output { path, .. } => Some(path.path.rsplit(['\\', '/']).next().unwrap().to_string()),
                _ => None,
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["fixture-abc.d", "libfixture-abc.rlib", "libfixture-abc.rmeta"]);
        let n = evs.len();
        assert!(matches!(evs[n - 1], ExecEvent::Exit(0)));
        assert!(matches!(evs[n - 2], ExecEvent::Output { .. }), "outputs come right before Exit");
        // Human diagnostics carry the client path too.
        let client_root = cp(&f.root).path;
        assert!(evs.iter().any(|e| matches!(e, ExecEvent::Stderr(l) if l.contains(&client_root))), "{evs:?}");
        let sandbox_root = f.jobs.to_string_lossy().replace('/', "\\");
        assert!(!evs.iter().any(|e| matches!(e, ExecEvent::Stderr(l) if l.contains(&sandbox_root))));
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
        assert!(matches!(evs.last(), Some(ExecEvent::Exit(code)) if *code != 0), "{evs:?}");
        assert!(evs.iter().any(|e| matches!(e, ExecEvent::Stderr(l) if l.contains("error"))));
        assert!(!evs.iter().any(|e| matches!(e, ExecEvent::Output { .. })), "outputs after a failed compile");
        assert!(!f.jobs.join(job_hex(&spec.job_id)).exists());
    }

    /// A stand-in "rustc" that just sleeps: a tiny program compiled with the
    /// real rustc, so the kill path is exercised without any CPU load.
    fn fake_sleeper(f: &Fixture) -> Installed {
        let dir = f._tmp.path().join("fake");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("sleeper.rs");
        std::fs::write(&src, "fn main() { std::thread::sleep(std::time::Duration::from_secs(120)); }").unwrap();
        let exe = dir.join(if cfg!(windows) { "sleeper.exe" } else { "sleeper" });
        let st = std::process::Command::new(&f.tc.rustc)
            .arg(&src)
            .arg("-o")
            .arg(&exe)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success());
        Installed { rustc: exe, ..f.tc.clone() }
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
        assert_eq!(err.downcast_ref::<ExecAborted>(), Some(&ExecAborted::Timeout), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(matches!(evs.first(), Some(ExecEvent::Started)));
        assert!(!evs.iter().any(|e| matches!(e, ExecEvent::Exit(_))));
        assert!(!f.jobs.join(job_hex(&spec.job_id)).exists(), "sandbox not deleted after timeout");
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
        assert_eq!(err.downcast_ref::<ExecAborted>(), Some(&ExecAborted::Cancelled));
        assert!(!f.jobs.join(job_hex(&spec.job_id)).exists());
    }

    #[test]
    fn missing_inputs_lists_absent_blobs_once() {
        let f = fixture();
        let mut spec = lib_spec(&f, SRC, false);
        let ghost = BlobHash::of(b"never stored");
        for n in ["a.rs", "b.rs"] {
            spec.inputs.push(InputFile { path: cp(&f.root.join(n)), hash: ghost, size: 12, executable: false });
        }
        let c = ctx(&f, Duration::from_secs(1));
        assert_eq!(missing_inputs(&c, &spec), vec![ghost]);
    }

    #[test]
    fn build_vars_are_filtered_system_vars_kept() {
        for n in ["CARGO_HOME", "cargo_pkg_name", "RUSTFLAGS", "RUSTC_WRAPPER", "RUSTDOCFLAGS", "OUT_DIR", "DEP_X_Y", "TARGET", "HOST", "PROFILE", "NUM_JOBS", "OPT_LEVEL", "DEBUG", "MAKEFLAGS", "MFLAGS", "CARGO_MAKEFLAGS"] {
            assert!(is_build_var(n), "{n}");
        }
        for n in ["PATH", "SystemRoot", "LIB", "INCLUDE", "RUSTUP_HOME", "TEMP", "HOME"] {
            assert!(!is_build_var(n), "{n}");
        }
    }

    #[test]
    fn rejects_escaping_paths() {
        let f = fixture();
        let mut spec = lib_spec(&f, SRC, false);
        spec.inputs[0].path = ClientPath { style: PathStyle::Posix, path: "/a/../../etc/x".into() };
        assert!(validate_paths(&spec).is_err());
    }

    #[tokio::test]
    async fn job_env_excludes_node_build_vars() {
        // Set in this process: if it leaked into rustc, `env!` in the source would see it.
        // SAFETY-free approach: use a variable name unique to this test and check via option_env!.
        let f = fixture();
        unsafe { std::env::set_var("CARGO_CRABFARM_LEAK_PROBE", "leaked") };
        let src = "pub const P: Option<&str> = option_env!(\"CARGO_CRABFARM_LEAK_PROBE\");\npub const Q: Option<&str> = option_env!(\"CARGO_CRABFARM_FROM_SPEC\");\n";
        let mut spec = lib_spec(&f, src, false);
        spec.env.push(("CARGO_CRABFARM_FROM_SPEC".into(), EnvValue::Plain("yes".into())));
        let c = ctx(&f, Duration::from_secs(120));
        let (res, evs) = collect(&c, &spec, &f.tc, CancellationToken::new()).await;
        res.unwrap();
        // option_env! reads are recorded as env-dep lines in the .d file.
        let dep = evs
            .iter()
            .find_map(|e| match e {
                ExecEvent::Output { path, data, .. } if path.path.ends_with(".d") => Some(String::from_utf8(data.clone()).unwrap()),
                _ => None,
            })
            .unwrap();
        assert!(dep.contains("# env-dep:CARGO_CRABFARM_LEAK_PROBE\n") || dep.contains("# env-dep:CARGO_CRABFARM_LEAK_PROBE\r\n"), "{dep}");
        assert!(!dep.contains("CARGO_CRABFARM_LEAK_PROBE=leaked"), "node variable leaked: {dep}");
        assert!(dep.contains("CARGO_CRABFARM_FROM_SPEC=yes"), "{dep}");
        unsafe { std::env::remove_var("CARGO_CRABFARM_LEAK_PROBE") };
    }
}
