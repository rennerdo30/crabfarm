//! crabfarm: distributed rustc over a peer-to-peer farm. See docs/DESIGN.md.

pub mod config;
pub mod proto;

// Package A: wrapper-side pure logic.
pub mod inputs;
pub mod paths;
pub mod rewrite;
pub mod rustc_args;
pub mod secfile;

// Package B: identity, transport, discovery.
pub mod discovery;
pub mod farm;
pub mod transport;

// Package C: executing peer.
pub mod cas;
pub mod ccache;
pub mod exec;
pub mod guard;
pub mod proctree;
pub mod toolchain;

// Package D (integration): node daemon, scheduler, IPC, wrapper flow,
// `crabfarm cargo`, `crabfarm status`, dependency graph.
pub mod cargo_cmd;
pub mod depgraph;
pub mod envlearn;
pub mod executor;
pub mod ipc;
pub mod link;
pub mod node;
pub mod scheduler;
pub mod status;
pub mod submitter;
pub mod wrapper;

// CF-22: install and setup commands (`node --stop`, `service`, `path`,
// `enable`/`disable`) and the console-less node binary's shared code.
pub mod daemon;
pub mod enable;
pub mod pathenv;
pub mod service;
// CF-37 / CF-38
pub mod doctor;
pub mod update;
#[cfg(windows)]
pub mod winreg;
