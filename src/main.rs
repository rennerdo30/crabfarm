//! crabfarm CLI. Wrapper mode is detected before clap parsing: cargo calls
//! `crabfarm <path-to-rustc> <args...>` when crabfarm is `RUSTC_WRAPPER`.

use clap::{Parser, Subcommand};
use std::path::Path;

#[derive(Parser)]
#[command(name = "crabfarm", version, about = "Distributed rustc over a peer-to-peer farm")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the node daemon (serves peers and local builds).
    Node,
    /// Create, join or invite to a farm.
    Farm {
        #[command(subcommand)]
        cmd: FarmCmd,
    },
    /// Run cargo with crabfarm as rustc wrapper and a farm-sized -j.
    #[command(trailing_var_arg = true, allow_hyphen_values = true)]
    Cargo { args: Vec<String> },
    /// Show peers, slots, running jobs and counters.
    Status,
}

#[derive(Subcommand)]
enum FarmCmd {
    /// Create a new farm on this machine.
    Create {
        #[arg(long)]
        name: Option<String>,
    },
    /// Print a one-time join code (10 minutes, single use).
    Invite,
    /// Join a farm with a code from `crabfarm farm invite`.
    Join { code: String },
}

fn is_wrapper_invocation(arg1: &str) -> bool {
    let stem = Path::new(arg1)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    stem == "rustc" || stem == "clippy-driver"
}

fn main() -> anyhow::Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() >= 2 && is_wrapper_invocation(&argv[1]) {
        // Package D: wrapper flow (classify, IPC submit, local fallback).
        let status = std::process::Command::new(&argv[1]).args(&argv[2..]).status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Node | Cmd::Farm { .. } | Cmd::Cargo { .. } | Cmd::Status => {
            anyhow::bail!("not implemented yet (package D)")
        }
    }
}
