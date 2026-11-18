//! crabfarm CLI. Wrapper mode is detected before clap parsing: cargo calls
//! `crabfarm <path-to-rustc> <args...>` when crabfarm is `RUSTC_WRAPPER`.

use anyhow::Context;
use clap::{Parser, Subcommand};
use crabfarm::config::{self, Config};
use crabfarm::discovery::PeerBook;
use crabfarm::farm::{self, Invites, JoinCode};
use crabfarm::{cargo_cmd, node, status, wrapper};
use std::path::Path;

#[derive(Parser)]
#[command(
    name = "crabfarm",
    version,
    about = "Distributed rustc over a peer-to-peer farm"
)]
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
    Invite {
        /// Address other machines reach this node at, `host:port`
        /// (default: first LAN IPv4 of this machine and the listen port).
        #[arg(long)]
        addr: Option<String>,
    },
    /// Join a farm with a code from `crabfarm farm invite`.
    Join {
        code: String,
        /// Name of this node in the farm (default: host name).
        #[arg(long)]
        name: Option<String>,
    },
}

fn is_wrapper_invocation(arg1: &str) -> bool {
    let stem = Path::new(arg1)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    stem == "rustc" || stem == "clippy-driver"
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() >= 2 && is_wrapper_invocation(&argv[1]) {
        std::process::exit(wrapper::run(&argv[1], &argv[2..]));
    }
    let code = match real_main() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("crabfarm: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("start async runtime")
}

fn real_main() -> anyhow::Result<i32> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Cargo { args } => Ok(cargo_cmd::run(&args)),
        Cmd::Status => Ok(status::run()),
        Cmd::Node => {
            let data_dir = config::data_dir()?;
            // First, before logging, the job sweep or any port: a second node on
            // the same directory must not touch anything of the first (CF-19).
            let lock = node::NodeLock::acquire(&data_dir)?;
            node::init_logging(&data_dir);
            let rt = runtime()?;
            rt.block_on(async {
                let token = tokio_util::sync::CancellationToken::new();
                let t2 = token.clone();
                tokio::spawn(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    t2.cancel();
                });
                node::run(data_dir, token, lock).await
            })?;
            Ok(0)
        }
        Cmd::Farm { cmd } => farm_cmd(cmd),
    }
}

fn farm_cmd(cmd: FarmCmd) -> anyhow::Result<i32> {
    let data_dir = config::data_dir()?;
    let cfg = Config::load(&data_dir)?;
    match cmd {
        FarmCmd::Create { name } => {
            let name = name.unwrap_or_else(|| cfg.node_name.clone());
            let id = farm::create(&data_dir, &name)?;
            println!(
                "Created farm {} with this machine as `{}`.",
                id.short_farm_id(),
                id.node_name
            );
            println!(
                "Next: run `crabfarm node` here, then `crabfarm farm invite` to add a machine."
            );
            Ok(0)
        }
        FarmCmd::Invite { addr } => {
            let id = farm::load(&data_dir)?
                .context("this machine is not in a farm: run `crabfarm farm create` first")?;
            anyhow::ensure!(
                id.ca_key_der.is_some(),
                "only the machine that created the farm can invite (P1)"
            );
            let addr = match addr {
                Some(a) => a,
                None => default_invite_addr(&data_dir, &cfg)?,
            };
            let mut invites = Invites::load(&data_dir)?;
            let token = invites.issue(&data_dir)?;
            let code = JoinCode {
                token,
                cert_fp: farm::cert_fingerprint(&id.node_cert_der),
                addr,
            };
            println!("{}", code.encode());
            eprintln!(
                "Valid for 10 minutes, single use. On the new machine run:\n  crabfarm farm join <code>"
            );
            Ok(0)
        }
        FarmCmd::Join { code, name } => {
            let code = JoinCode::parse(&code)?;
            let name = name.unwrap_or_else(|| cfg.node_name.clone());
            let rt = runtime()?;
            let (id, known) = rt.block_on(farm::join(&data_dir, &name, &code))?;
            let mut book = PeerBook::load(&data_dir)?;
            book.merge(&known, id.node_id);
            book.save(&data_dir)?;
            println!("Joined farm {} as `{}`.", id.short_farm_id(), id.node_name);
            println!(
                "Next: run `crabfarm node` here, then `crabfarm cargo build` in a Rust project."
            );
            Ok(0)
        }
    }
}

/// First LAN IPv4 of this machine plus the node's port: the one written by a
/// running node (`listen.addr`), else the configured one.
fn default_invite_addr(data_dir: &Path, cfg: &Config) -> anyhow::Result<String> {
    let listen = std::fs::read_to_string(data_dir.join("listen.addr"))
        .unwrap_or_else(|_| cfg.listen.clone());
    let port = listen
        .trim()
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .filter(|p| *p != 0)
        .context("cannot tell the listen port; pass --addr host:port")?;
    let ip = node::lan_ipv4s()
        .into_iter()
        .next()
        .context("no LAN address found; pass --addr host:port")?;
    Ok(format!("{ip}:{port}"))
}
