//! crabfarm: distributed rustc over a peer-to-peer farm. See docs/DESIGN.md.

pub mod config;
pub mod proto;

// Package A: wrapper-side pure logic.
pub mod inputs;
pub mod paths;
pub mod rewrite;
pub mod rustc_args;

// Package B: identity, transport, discovery.
pub mod discovery;
pub mod farm;
pub mod transport;

// Package C: executing peer.
pub mod cas;
pub mod exec;
pub mod guard;
pub mod proctree;
pub mod toolchain;

// Package D (integration): node daemon, scheduler, IPC, wrapper flow,
// `crabfarm cargo`, `crabfarm status`, dependency graph.
pub mod cargo_cmd;
pub mod depgraph;
pub mod executor;
pub mod ipc;
pub mod link;
pub mod node;
pub mod scheduler;
pub mod status;
pub mod submitter;
pub mod wrapper;
