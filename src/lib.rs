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
pub mod toolchain;

// Package D (integration): node daemon, scheduler, IPC, wrapper flow,
// `crabfarm cargo`, `crabfarm status`, dependency graph. Not started yet.
