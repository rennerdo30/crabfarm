//! Package C. Run one job on the executing peer (DESIGN section 9).
//!
//! Transport-agnostic: the caller (node, package D) moves frames; this module
//! materializes the sandbox, runs rustc under the node jobserver, and emits
//! [`ExecEvent`]s in the order the submitter must apply them.

use crate::cas::BlobStore;
use crate::proto::{BlobHash, ClientPath, JobSpec, NodeId};
use crate::toolchain::Installed;
use std::path::PathBuf;

/// Node-wide jobserver shared by all rustc processes on this peer.
pub struct NodeJobserver {
    pub client: jobserver::Client,
}

impl NodeJobserver {
    pub fn new(slots: u32) -> anyhow::Result<NodeJobserver> {
        let _ = slots;
        todo!("package C")
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
    let _ = (ctx, spec);
    todo!("package C")
}

/// Materialize the sandbox (all inputs must be in the store now), run rustc
/// (`toolchain.rustc`) with mapped args + remaps + jobserver, stream events to
/// `events`, collect outputs from the mapped out-dir, delete the sandbox.
/// `cancel` aborts the job (kills only the rustc child it spawned).
pub async fn run(
    ctx: &ExecContext<'_>,
    spec: &JobSpec,
    toolchain: &Installed,
    events: tokio::sync::mpsc::Sender<ExecEvent>,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let _ = (ctx, spec, toolchain, events, cancel);
    todo!("package C")
}
