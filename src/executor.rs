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

/// How long a reserved slot waits for the `JobRequest`.
const REQUEST_WAIT: Duration = Duration::from_secs(30);
/// Silence while the submitter uploads blobs.
const BLOB_WAIT: Duration = Duration::from_secs(60);

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
        // Caps, arguments and environment are checked before any blob is
        // received or written (CF-8); a refusal makes the submitter run locally.
        exec::validate_spec(
            &spec,
            &self.cfg.accept_env,
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
                    crate_name = spec.crate_name.as_str(),
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
        let mut want: HashMap<BlobHash, u64> = HashMap::new();
        for h in missing {
            if let Some(i) = spec.inputs.iter().find(|i| i.hash == *h) {
                want.insert(*h, i.size);
            }
        }
        let mut asm: HashMap<BlobHash, Assembler> = HashMap::new();
        while !want.is_empty() {
            let m = tokio::time::timeout(BLOB_WAIT, link.recv())
                .await
                .context("blob upload stalled")?;
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
                        self.store.put(owner, key, &hash, &plain)?;
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
