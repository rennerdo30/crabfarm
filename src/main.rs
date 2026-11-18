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
    Node {
        /// Ask the running node to shut down cleanly, then exit.
        #[arg(long)]
        stop: bool,
    },
    /// Start the node when you sign in (per user, no admin rights).
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// Put crabfarm on your PATH (Windows; elsewhere prints the line to add).
    Path {
        #[command(subcommand)]
        cmd: PathCmd,
    },
    /// Make plain `cargo build` use the farm in this project (or everywhere).
    Enable {
        /// Write to `$CARGO_HOME/config.toml` instead of this project.
        #[arg(long)]
        global: bool,
        /// `build.jobs` to write (default: local slots plus free remote slots).
        #[arg(long)]
        jobs: Option<u32>,
    },
    /// Undo `crabfarm enable`.
    Disable {
        #[arg(long)]
        global: bool,
        /// Only when the entry points at THIS crabfarm executable (the
        /// uninstaller uses it so another install's entry stays).
        #[arg(long)]
        this_install_only: bool,
    },
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
enum ServiceCmd {
    /// Turn autostart on (and start the node now if it is not running).
    Install,
    /// Turn autostart off.
    Uninstall,
    /// Show whether autostart is on and the node is running.
    Status,
}

#[derive(Subcommand)]
enum PathCmd {
    /// Add the crabfarm folder to your PATH.
    Add,
    /// Remove it again.
    Remove,
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

fn stop_cmd() -> anyhow::Result<i32> {
    let data_dir = config::data_dir()?;
    if crabfarm::daemon::stop_node(&data_dir, crabfarm::daemon::STOP_TIMEOUT)? {
        println!("The node has stopped.");
    } else {
        println!("No node is running.");
    }
    Ok(0)
}

fn service_cmd(cmd: ServiceCmd) -> anyhow::Result<i32> {
    use crabfarm::service;
    let target = service::current_target()?;
    let mut host = service::RealHost::new(target.data_dir.clone());
    match cmd {
        ServiceCmd::Install => {
            for l in service::install(&mut host, &target)? {
                println!("{l}");
            }
        }
        ServiceCmd::Uninstall => {
            for l in service::uninstall(&mut host, &target)? {
                println!("{l}");
            }
        }
        ServiceCmd::Status => {
            let st = service::status(&host, &target)?;
            println!(
                "autostart: {}{}",
                if st.autostart { "on" } else { "off" },
                if st.stale {
                    " (points at another crabfarm install; run `crabfarm service install` to fix)"
                } else {
                    ""
                }
            );
            println!(
                "node: {}",
                if st.node_running {
                    "running"
                } else {
                    "not running"
                }
            );
        }
    }
    Ok(0)
}

fn path_cmd(cmd: PathCmd) -> anyhow::Result<i32> {
    let exe = std::env::current_exe().context("cannot find my own executable")?;
    let dir = exe.parent().context("crabfarm has no parent directory")?;
    #[cfg(windows)]
    {
        use crabfarm::pathenv::{self, Change};
        let dir = dir.to_string_lossy().into_owned();
        let mut env = pathenv::RegistryEnv;
        let (changed, done, already) = match cmd {
            PathCmd::Add => (
                pathenv::add(&mut env, &dir)?,
                "Added to your PATH",
                "Already on your PATH",
            ),
            PathCmd::Remove => (
                pathenv::remove(&mut env, &dir)?,
                "Removed from your PATH",
                "Was not on your PATH",
            ),
        };
        match changed {
            Change::Changed => println!("{done}: {dir}\nOpen a new terminal to see it."),
            Change::Unchanged => println!("{already}: {dir}"),
        }
    }
    #[cfg(not(windows))]
    {
        let line = crabfarm::pathenv::shell_line(dir);
        match cmd {
            PathCmd::Add => println!(
                "crabfarm does not edit shell profiles. Add this line to your shell profile (~/.profile, ~/.zshrc or ~/.bashrc):\n  {line}"
            ),
            PathCmd::Remove => println!(
                "crabfarm never edited a shell profile. Remove this line from yours if you added it:\n  {line}"
            ),
        }
    }
    Ok(0)
}

/// With autostart on, a node that started before this machine had a farm has
/// exited; start it now (see `service::start_after_setup`).
fn start_node_after_setup() {
    use crabfarm::service;
    let Ok(target) = service::current_target() else {
        return;
    };
    let mut host = service::RealHost::new(target.data_dir.clone());
    match service::start_after_setup(&mut host, &target) {
        Ok(Some(line)) => println!("{line}"),
        Ok(None) => {}
        Err(e) => eprintln!("crabfarm: could not start the node: {e:#}"),
    }
}

fn cargo_home() -> Option<std::path::PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().join(".cargo")))
}

fn enable_cmd(global: bool, jobs: Option<u32>) -> anyhow::Result<i32> {
    use crabfarm::enable;
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let file = enable::target_config(global, &cwd, cargo_home().as_deref())?;
    let exe = std::env::current_exe().context("cannot find my own executable")?;
    let data_dir = config::data_dir()?;
    let cfg = Config::load(&data_dir).unwrap_or_default();
    let status = cargo_cmd::query_status(&data_dir);
    let peers = match &status {
        Some(crabfarm::proto::Message::FarmStatus { peers, .. }) => Some(peers.as_slice()),
        _ => None,
    };
    let default = enable::default_jobs(cfg.local_slots, peers);
    // The wrapper cargo uses today for this scope, found with cargo's own
    // precedence and without the environment (a variable is not something to
    // copy into a file; it is warned about below).
    let outer = if global {
        None
    } else {
        cargo_cmd::find_inner_wrapper(&[], &cwd, cargo_home().as_deref(), &|_| None, Some(&exe))
    };
    let r = enable::enable_file(&file, &exe, jobs, default, outer.as_deref())?;
    if !global && let Some(root) = file.parent().and_then(Path::parent) {
        println!(
            "Project root: {} (the workspace root when this is a workspace member).",
            root.display()
        );
    }
    println!("crabfarm is now the rustc wrapper in {}", r.file.display());
    if let Some(w) = &r.inner {
        println!(
            "Your earlier wrapper `{w}` stays in use for local compiles (kept as [env] {}).",
            cargo_cmd::INNER_WRAPPER_ENV
        );
    }
    match r.jobs {
        Some((n, true)) => println!(
            "build.jobs = {n} ({}). Change it with `crabfarm enable --jobs N` or edit that line.",
            if jobs.is_some() {
                "your choice"
            } else if status.is_some() {
                "your local slots plus the free slots in the farm right now"
            } else {
                "twice your local slots; the node is not running, so the farm size is unknown"
            }
        ),
        Some((n, false)) => println!(
            "build.jobs = {n} is your own value and was left alone; `crabfarm enable --jobs N` replaces it."
        ),
        None => {}
    }
    if let Some(name) = enable::env_wrapper_override(&|n| std::env::var_os(n)) {
        eprintln!(
            "WARNING: {name} is set in your environment. Cargo lets it override the config file, \
             so crabfarm is NOT used until you unset {name} (a file cannot change that)."
        );
    }
    println!(
        "Plain `cargo build` in this scope now goes through the farm. Undo with `crabfarm disable`."
    );
    Ok(0)
}

fn disable_cmd(global: bool, this_install_only: bool) -> anyhow::Result<i32> {
    use crabfarm::enable;
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let file = enable::target_config(global, &cwd, cargo_home().as_deref())?;
    let exe = std::env::current_exe().context("cannot find my own executable")?;
    let r = enable::disable_file_if(&file, this_install_only.then_some(exe.as_path()))?;
    if !r.was_enabled {
        println!(
            "crabfarm was not enabled in {}; nothing changed.",
            r.file.display()
        );
    } else {
        println!("crabfarm is off in {}.", r.file.display());
        if let Some(w) = r.restored {
            println!("Your earlier wrapper `{w}` is back.");
        }
    }
    Ok(0)
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
        Cmd::Node { stop: true } => stop_cmd(),
        Cmd::Node { stop: false } => {
            crabfarm::daemon::run_node()?;
            Ok(0)
        }
        Cmd::Service { cmd } => service_cmd(cmd),
        Cmd::Path { cmd } => path_cmd(cmd),
        Cmd::Enable { global, jobs } => enable_cmd(global, jobs),
        Cmd::Disable {
            global,
            this_install_only,
        } => disable_cmd(global, this_install_only),
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
            start_node_after_setup();
            println!(
                "Next: make sure the node runs (`crabfarm service status`; `crabfarm node` starts it in this terminal), then `crabfarm farm invite` to add a machine."
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
            // The inviter's list is gossip too (CF-19); loopback addresses only
            // make sense when the code itself pointed at this machine.
            let known = crabfarm::discovery::sanitize_gossip(
                &known,
                crabfarm::discovery::is_loopback_peer(&code.addr),
            );
            book.merge(&known, id.node_id);
            book.save(&data_dir)?;
            println!("Joined farm {} as `{}`.", id.short_farm_id(), id.node_name);
            start_node_after_setup();
            println!(
                "Next: make sure the node runs (`crabfarm service status`; `crabfarm node` starts it in this terminal), then run `crabfarm enable` in a Rust project."
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
