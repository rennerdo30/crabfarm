//! Executing side of a job connection (DESIGN section 9): `Reserve` ->
//! `JobRequest` -> blobs -> rustc -> frames back to the submitter.

use crate::cas::{self, Assembler};
use crate::exec::{self, ExecAborted, ExecContext, ExecEvent};
use crate::link::Link;
use crate::node::Node;
use crate::proto::{BlobHash, JobId, JobSpec, Message, NodeId};
use anyhow::{Context, bail};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The executor sends `Message::Keepalive` this often while a job is on its
/// slot; the submitter gives up on 30 s without any frame (CF-25).
pub const HEARTBEAT: Duration = Duration::from_secs(5);
/// How long a reserved slot waits for the `JobRequest`.
const REQUEST_WAIT: Duration = Duration::from_secs(30);
/// Silence while the submitter uploads blobs.
const BLOB_WAIT: Duration = Duration::from_secs(60);
/// The whole upload of one job (CF-19).
const BLOB_TOTAL: Duration = Duration::from_secs(600);

impl Node {
    /// Handle one job connection whose first frame was `Reserve`.
    pub async fn serve_job(self: &Arc<Self>, submitter: NodeId, mut link: Link, job_id: JobId) {
        let Some(slot) = self.reserve_slot().filter(|_| self.cfg.accept_remote_jobs) else {
            let _ = link.send(Message::Busy { job_id }).await;
            link.close().await;
            return;
        };
        let result = self.serve_reserved(submitter, &mut link, job_id).await;
        if let Err(e) = result {
            tracing::debug!("job connection failed: {e:#}");
            let _ = link
                .send(Message::Error {
                    message: format!("{e:#}"),
                })
                .await;
        }
        drop(slot);
        link.close().await;
    }

    async fn serve_reserved(
        self: &Arc<Self>,
        submitter: NodeId,
        link: &mut Link,
        job_id: JobId,
    ) -> anyhow::Result<()> {
        link.send(Message::Reserved { job_id }).await?;
        let first = tokio::time::timeout(REQUEST_WAIT, link.recv())
            .await
            .context("no JobRequest after Reserve")?;
        let Some(Message::JobRequest { spec, blob_key }) = first else {
            bail!("expected JobRequest")
        };
        if spec.job_id != job_id {
            bail!("JobRequest for a different job");
        }
        // From here to the end of the job the submitter hears from this node
        // every few seconds, whatever rustc does: its per-frame idle limit is
        // how it notices a machine that froze or lost its network (CF-25).
        let _heartbeat = link.keepalive_ticker(HEARTBEAT, || true);
        // Caps, arguments and environment are checked before any blob is
        // received or written (CF-8); a refusal makes the submitter run locally.
        exec::validate_spec(
            &spec,
            &self.cfg.accept_env,
            self.cfg.accept_build_env,
            &self.host_triple,
            &self.data_dir.join("jobs"),
        )?;
        // From here on no other job's cleanup or an eviction may delete a blob
        // this job needs, including the ones still to be uploaded.
        let _pin = self
            .store
            .pin(submitter, spec.inputs.iter().map(|i| i.hash));

        let timeout = Duration::from_secs(self.cfg.job_timeout_min.max(1) * 60);
        let ctx = ExecContext {
            jobs_dir: self.data_dir.join("jobs"),
            store: &self.store,
            jobserver: &self.jobserver,
            owner: submitter,
            key: blob_key,
            timeout,
            accept_env: &self.cfg.accept_env,
            accept_build_env: self.cfg.accept_build_env,
        };
        let missing = exec::missing_inputs(&ctx, &spec);
        if !missing.is_empty() {
            link.send(Message::NeedBlobs {
                hashes: missing.clone(),
            })
            .await?;
            self.receive_blobs(link, submitter, &blob_key, &spec, &missing)
                .await?;
        }

        let Some(toolchain) = self.toolchains.resolve(&spec.toolchain, &spec.target) else {
            // The submitter falls back; resolve() has started an install if allowed.
            self.status_changed();
            bail!("toolchain or target not available on this node");
        };

        let started = Instant::now();
        let cancel = CancellationToken::new();
        let (etx, mut erx) = mpsc::channel::<ExecEvent>(64);
        let node = self.clone();
        let spec2 = spec.clone();
        let cancel2 = cancel.clone();
        let run = tokio::spawn(async move {
            let ctx = ExecContext {
                jobs_dir: node.data_dir.join("jobs"),
                store: &node.store,
                jobserver: &node.jobserver,
                owner: submitter,
                key: blob_key,
                timeout,
                accept_env: &node.cfg.accept_env,
                accept_build_env: node.cfg.accept_build_env,
            };
            exec::run(&ctx, &spec2, &toolchain, etx, cancel2).await
        });

        // rustc may be silent for long; the job timeout bounds this phase.
        link.set_idle(None);
        let mut peer_gone = false;
        let mut exit_code: Option<i32> = None;
        loop {
            tokio::select! {
                ev = erx.recv() => {
                    let Some(ev) = ev else { break };
                    let sent = match ev {
                        ExecEvent::Started => link.send(Message::Started).await,
                        ExecEvent::Stderr(line) => link.send(Message::Stderr { line }).await,
                        ExecEvent::Stdout(line) => link.send(Message::Stdout { line }).await,
                        ExecEvent::Output { path, hash, data } => send_output(link, path, hash, &data).await,
                        ExecEvent::Tainted(path) => link.send(Message::Tainted { path }).await,
                        ExecEvent::Exit(code) => {
                            exit_code = Some(code);
                            link.send(Message::Exit { code }).await
                        }
                    };
                    if sent.is_err() {
                        cancel.cancel();
                        peer_gone = true;
                    }
                }
                m = link.recv(), if !peer_gone => match m {
                    Some(Message::Cancel) | None => {
                        peer_gone = true;
                        cancel.cancel();
                    }
                    Some(_) => {}
                },
                _ = self.shutdown.cancelled() => cancel.cancel(),
            }
        }
        let outcome = run.await.context("job task panicked")?;
        self.count("served", 1);
        match outcome {
            Ok(()) => {
                tracing::info!(
                    crate_name = crate::proto::log_safe(&spec.crate_name).as_str(),
                    code = exit_code.unwrap_or(-1),
                    ms = started.elapsed().as_millis() as u64,
                    "served job"
                );
                Ok(())
            }
            Err(e) => match e.downcast_ref::<ExecAborted>() {
                // The submitter was told (`Tainted`) and reruns the job itself.
                Some(ExecAborted::Cancelled | ExecAborted::Tainted) => Ok(()),
                Some(ExecAborted::Timeout) => bail!("job timed out"),
                None => Err(e),
            },
        }
    }

    /// Receive exactly the requested blobs into the store.
    async fn receive_blobs(
        &self,
        link: &mut Link,
        owner: NodeId,
        key: &[u8; 32],
        spec: &JobSpec,
        missing: &[BlobHash],
    ) -> anyhow::Result<()> {
        receive_blobs_into(link, spec, missing, BLOB_WAIT, BLOB_TOTAL, |hash, plain| {
            self.store.put(owner, key, hash, plain)
        })
        .await
    }
}

/// Receive the `missing` blobs of `spec` from `link`, handing each complete one
/// to `put`. Two limits apply (CF-19): no frame may take longer than
/// `frame_wait`, and the whole upload may not take longer than `total_wait`, so
/// a submitter that sends one chunk every few seconds cannot hold a reserved
/// slot for ever.
async fn receive_blobs_into(
    link: &mut Link,
    spec: &JobSpec,
    missing: &[BlobHash],
    frame_wait: Duration,
    total_wait: Duration,
    mut put: impl FnMut(&BlobHash, &[u8]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut want: HashMap<BlobHash, u64> = HashMap::new();
    for h in missing {
        if let Some(i) = spec.inputs.iter().find(|i| i.hash == *h) {
            want.insert(*h, i.size);
        }
    }
    let mut asm: HashMap<BlobHash, Assembler> = HashMap::new();
    let deadline = Instant::now() + total_wait;
    while !want.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("blob upload exceeded its overall deadline");
        }
        let m = match tokio::time::timeout(frame_wait.min(left), link.recv()).await {
            Ok(m) => m,
            Err(_) if Instant::now() >= deadline => {
                bail!("blob upload exceeded its overall deadline")
            }
            Err(_) => bail!("blob upload stalled"),
        };
        match m {
            Some(Message::BlobChunk {
                hash,
                offset,
                last,
                data,
            }) => {
                let size = *want
                    .get(&hash)
                    .context("peer sent a blob that was not requested")?;
                if offset > size {
                    bail!("blob chunk beyond the declared size");
                }
                let a = asm.entry(hash).or_default();
                if let Some(plain) = a.push(&hash, offset, last, &data)? {
                    if plain.len() as u64 != size {
                        bail!("blob size differs from the job's declaration");
                    }
                    put(&hash, &plain)?;
                    want.remove(&hash);
                    asm.remove(&hash);
                }
            }
            Some(Message::Cancel) | None => bail!("submitter went away during upload"),
            Some(_) => bail!("unexpected message during blob upload"),
        }
    }
    Ok(())
}

/// `Output` followed by its chunks; the submitter writes the file before it
/// sees any later frame.
async fn send_output(
    link: &Link,
    path: crate::proto::ClientPath,
    hash: BlobHash,
    data: &[u8],
) -> anyhow::Result<()> {
    link.send(Message::Output {
        path,
        hash,
        size: data.len() as u64,
    })
    .await?;
    for (offset, last, chunk) in cas::chunks(data) {
        link.send(Message::BlobChunk {
            hash,
            offset,
            last,
            data: chunk,
        })
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{ClientPath, InputFile, PathStyle, ToolchainId};
    use crate::transport::FramedConn;

    fn pair() -> (Link, FramedConn<tokio::io::DuplexStream>) {
        let (a, b) = tokio::io::duplex(1 << 20);
        (Link::new(FramedConn::new(a)), FramedConn::new(b))
    }

    fn spec_with(data: &[u8]) -> (JobSpec, BlobHash) {
        let hash = BlobHash::of(data);
        let p = ClientPath::parse(PathStyle::Posix, "/w/a.rs").unwrap();
        let spec = JobSpec {
            job_id: JobId([1; 16]),
            toolchain: ToolchainId {
                release: "1".into(),
                commit_hash: "a".into(),
                commit_date: "d".into(),
                host: "h".into(),
            },
            target: "t".into(),
            args: Vec::new(),
            cwd: p.clone(),
            env: Vec::new(),
            extra_env: Vec::new(),
            build_env: Vec::new(),
            inputs: vec![InputFile {
                path: p.clone(),
                hash,
                size: data.len() as u64,
                executable: false,
            }],
            dirs: Vec::new(),
            out_dir: p,
            crate_name: "x".into(),
            no_retain: false,
            needs_host: false,
        };
        (spec, hash)
    }

    /// CF-19: valid chunks, each well inside the per-frame wait, that add up to
    /// more than the overall deadline are cut off.
    #[tokio::test]
    async fn an_upload_that_keeps_going_past_the_deadline_is_refused() {
        // Many tiny blobs are not possible with one input; use one blob whose
        // chunks arrive one per 150 ms and whose end never comes within 500 ms.
        let data = vec![1u8; 3 * crate::proto::CHUNK_SIZE];
        let (spec, hash) = spec_with(&data);
        let (mut link, mut peer) = pair();
        let feeder = tokio::spawn(async move {
            for (offset, last, chunk) in crate::cas::chunks(&data) {
                tokio::time::sleep(Duration::from_millis(400)).await;
                let m = Message::BlobChunk {
                    hash,
                    offset,
                    last,
                    data: chunk,
                };
                if peer.send(&m).await.is_err() {
                    return;
                }
            }
            // Keep the connection open.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let r = receive_blobs_into(
            &mut link,
            &spec,
            &[hash],
            Duration::from_secs(30),
            Duration::from_millis(500),
            |_, _| Ok(()),
        )
        .await;
        feeder.abort();
        let err = format!("{:#}", r.unwrap_err());
        assert!(err.contains("deadline"), "{err}");
    }

    #[tokio::test]
    async fn a_prompt_upload_completes() {
        let data = vec![3u8; 100];
        let (spec, hash) = spec_with(&data);
        let (mut link, mut peer) = pair();
        for (offset, last, chunk) in crate::cas::chunks(&data) {
            peer.send(&Message::BlobChunk {
                hash,
                offset,
                last,
                data: chunk,
            })
            .await
            .unwrap();
        }
        let mut got = Vec::new();
        receive_blobs_into(
            &mut link,
            &spec,
            &[hash],
            Duration::from_secs(5),
            Duration::from_secs(5),
            |h, plain| {
                got.push((*h, plain.to_vec()));
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(got, vec![(hash, data)]);
    }
}
