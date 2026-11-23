//! `crabfarm doctor` (CF-38): look at everything that makes a farm build work
//! on this machine, say what is wrong in one line each with the way out, and
//! with `--fix` repair what is safe to repair here (start the node, point
//! autostart at this install, drop a stale "node did not answer" marker).
//!
//! Facts are gathered first ([`gather`]), the findings come from them alone
//! ([`diagnose`], tested), so the checks never change anything by looking.
//! Nothing here edits PATH, the environment, the firewall or a project file.

use crate::cargo_cmd::{self, WrapperSource};
use crate::node::Reach;
use crate::proto::{Message, PeerInfo, Status};
use crate::{config, daemon, farm, ipc, service};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Note,
    Warn,
    Problem,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fix {
    StartNode,
    RepairAutostart,
    ClearUnresponsive,
}

#[derive(Debug)]
pub struct Finding {
    pub level: Level,
    pub text: String,
    /// What the user does about it (shown under the finding).
    pub hint: Option<String>,
    /// What `--fix` does about it.
    pub fix: Option<Fix>,
}

fn f(level: Level, text: impl Into<String>) -> Finding {
    Finding {
        level,
        text: text.into(),
        hint: None,
        fix: None,
    }
}

impl Finding {
    fn hint(mut self, h: impl Into<String>) -> Finding {
        self.hint = Some(h.into());
        self
    }
    fn fix(mut self, x: Fix) -> Finding {
        self.fix = Some(x);
        self
    }
}

/// Libraries under a project's target directory, against the node's records.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Libs {
    pub dir: PathBuf,
    pub total: usize,
    /// Never built through crabfarm.
    pub unrecorded: usize,
    /// Built through crabfarm, changed on disk since.
    pub changed: usize,
}

#[derive(Debug, Default)]
pub struct Project {
    pub root: PathBuf,
    /// The wrapper cargo's config files name (environment left out).
    pub config_wrapper: Option<(String, PathBuf)>,
    pub libs: Option<Libs>,
}

pub struct NodeView {
    pub local: Status,
    pub peers: Vec<(PeerInfo, Option<Status>)>,
    pub counters: Vec<(String, u64)>,
}

#[derive(Default)]
pub struct Facts {
    pub exe: PathBuf,
    pub in_farm: bool,
    pub created_farm: bool,
    pub node_running: bool,
    pub node: Option<NodeView>,
    pub unresponsive: bool,
    /// `None` when autostart does not apply (a custom CRABFARM_HOME) or
    /// cannot be read.
    pub autostart: Option<service::Status>,
    /// The `crabfarm` a terminal finds first on PATH.
    pub on_path: Option<PathBuf>,
    /// `RUSTC_WRAPPER` / `CARGO_BUILD_RUSTC_WRAPPER` set to something that is
    /// not crabfarm: (variable, value).
    pub env_wrappers: Vec<(String, String)>,
    pub project: Option<Project>,
    pub listen_port: Option<u16>,
    /// Path to `<data>/logs/node.log`, shown as a note when problems or
    /// warnings exist (CF-45).
    pub log_path: Option<PathBuf>,
}

fn counter(c: &[(String, u64)], name: &str) -> u64 {
    c.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| *v)
        .unwrap_or(0)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => {
            if cfg!(windows) {
                a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
            } else {
                a == b
            }
        }
    }
}

// ------------------------------------------------------------------ findings

pub fn diagnose(x: &Facts) -> Vec<Finding> {
    let mut out = Vec::new();
    let port = x.listen_port.unwrap_or(47070);

    // Farm membership.
    if x.in_farm {
        out.push(f(
            Level::Ok,
            if x.created_farm {
                "This machine is in a farm (it created it)."
            } else {
                "This machine is in a farm (it joined it)."
            },
        ));
    } else {
        out.push(
            f(Level::Problem, "This machine is in no farm yet.").hint(
                "On the first machine run `crabfarm farm create`. On every other one, run `crabfarm farm invite` on a machine \
                 of the farm and `crabfarm farm join <code>` here.",
            ),
        );
    }

    // The node.
    match (&x.node, x.node_running) {
        (Some(n), _) => {
            out.push(f(
                Level::Ok,
                format!(
                    "The node runs: {} ({}), {} slots for peers.",
                    n.local.node_name, n.local.host_triple, n.local.slots_total
                ),
            ));
            if x.unresponsive {
                out.push(
                    f(
                        Level::Warn,
                        "Wrappers skip the node for a while because it did not answer one of them a moment ago.",
                    )
                    .hint("It answers now; `--fix` drops that marker so builds use the farm again at once.")
                    .fix(Fix::ClearUnresponsive),
                );
            }
        }
        (None, true) => out.push(
            f(
                Level::Problem,
                "A node holds the data directory but does not answer (an older crabfarm version, or it hangs).",
            )
            .hint("Run `crabfarm node --stop`, then `crabfarm doctor --fix` to start the node of this install."),
        ),
        (None, false) => {
            let mut finding = f(
                Level::Problem,
                "The node is not running, so every rustc runs locally.",
            )
            .hint("Start it with `crabfarm node` (or `crabfarm service install` to start it at every sign-in).");
            if x.in_farm {
                finding = finding.fix(Fix::StartNode);
            }
            out.push(finding);
        }
    }

    // Autostart.
    if let Some(a) = &x.autostart {
        if a.stale {
            out.push(
                f(
                    Level::Warn,
                    "Autostart starts a different crabfarm install than this one.",
                )
                .hint("`crabfarm service install` points it at this install (`--fix` does that).")
                .fix(Fix::RepairAutostart),
            );
        } else if !a.autostart {
            out.push(f(
                Level::Note,
                "Autostart is off: after a restart the node runs only once you start it (`crabfarm service install` turns it on).",
            ));
        }
    }

    // PATH.
    match &x.on_path {
        None => out.push(
            f(
                Level::Warn,
                "`crabfarm` is not on PATH, so terminals do not find it.",
            )
            .hint(if cfg!(windows) {
                "Run `crabfarm path add` with this crabfarm's full path, then open a new terminal."
            } else {
                "Run `crabfarm path add` for the line to put into your shell profile."
            }),
        ),
        Some(p) if !same_file(p, &x.exe) => out.push(
            f(
                Level::Warn,
                format!(
                    "The `crabfarm` on PATH is another install: {} (this one is {}).",
                    p.display(),
                    x.exe.display()
                ),
            )
            .hint(
                "Keep one install: uninstall the other, or run the one you want by its full path.",
            ),
        ),
        Some(_) => {}
    }

    // A wrapper in the environment wins over every config file.
    for (var, value) in &x.env_wrappers {
        out.push(
            f(
                Level::Problem,
                format!(
                    "{var}={value} is set in the environment, so plain `cargo build` runs `{value}` and never crabfarm, \
                     whatever `crabfarm enable` wrote. Crates cargo builds that way are also unknown to crabfarm, and \
                     the farm keeps everything that depends on them local (UnknownDependency)."
                ),
            )
            .hint(format!(
                "Build with `crabfarm cargo build` (it keeps `{value}` for local compiles), or put `{value}` behind crabfarm \
                 for good: set CRABFARM_INNER_WRAPPER={value}, remove {var}{}, open a new terminal and run `crabfarm enable --global`.",
                if cfg!(windows) {
                    " (Settings > System > About > Advanced system settings > Environment Variables; it may be set for the user and for the machine)"
                } else {
                    " from your shell profile"
                }
            )),
        );
    }

    // This project.
    if let Some(p) = &x.project {
        match &p.config_wrapper {
            None => out.push(f(
                Level::Note,
                format!(
                    "Plain `cargo build` in {} does not use crabfarm: no config names a rustc wrapper. \
                     `crabfarm enable` (this project) or `crabfarm enable --global` sets it; `crabfarm cargo build` works without.",
                    p.root.display()
                ),
            )),
            Some((w, file)) if cargo_cmd::is_crabfarm(w, Some(&x.exe)) => {
                if !Path::new(w).is_file() && Path::new(w).is_absolute() {
                    out.push(
                        f(
                            Level::Problem,
                            format!(
                                "{} names crabfarm at {w}, which does not exist: cargo fails there.",
                                file.display()
                            ),
                        )
                        .hint("Run `crabfarm enable` again (with `--global` when that file is the global one)."),
                    );
                } else if Path::new(w).is_absolute() && !same_file(Path::new(w), &x.exe) {
                    out.push(
                        f(
                            Level::Warn,
                            format!(
                                "{} names another crabfarm install: {w}.",
                                file.display()
                            ),
                        )
                        .hint("Run `crabfarm enable` again with the install you want to use."),
                    );
                } else if x.env_wrappers.is_empty() {
                    out.push(f(
                        Level::Ok,
                        format!("Plain `cargo build` here uses crabfarm ({}).", file.display()),
                    ));
                }
            }
            Some((w, file)) if w.is_empty() => out.push(f(
                Level::Note,
                format!("{} switches the rustc wrapper off for this project.", file.display()),
            )),
            Some((w, file)) => out.push(f(
                Level::Note,
                format!(
                    "Plain `cargo build` here uses `{w}` ({}), not crabfarm. `crabfarm enable` puts crabfarm in front and keeps `{w}` for local compiles.",
                    file.display()
                ),
            )),
        }
        if let Some(l) = &p.libs {
            let unknown = l.unrecorded + l.changed;
            let unknown_jobs = x
                .node
                .as_ref()
                .map(|n| counter(&n.counters, "local.UnknownDependency"))
                .unwrap_or(0);
            if l.total > 0 && unknown > 0 {
                let hint = if unknown_jobs > 0 || l.changed > 0 {
                    "Workspace crates built with incremental compilation (`remote_incremental = \"local\"`) \
                     and `cargo clippy` outputs stay local by design and are not recorded. Only if registry \
                     dependencies were built before enabling crabfarm, run `cargo clean` and rebuild once \
                     through crabfarm."
                } else {
                    "No job fell back for `UnknownDependency` in this node session: these are usually \
                     workspace crates built with incremental compilation (`remote_incremental = \"local\"`) \
                     or `cargo clippy` outputs, which stay local by design."
                };
                out.push(
                    f(
                        if unknown_jobs > 0 { Level::Warn } else { Level::Note },
                        format!(
                            "{} of {} libraries in {} were built without crabfarm{}. Crates that depend on them build locally (UnknownDependency).",
                            unknown,
                            l.total,
                            l.dir.display(),
                            if l.changed > 0 {
                                format!(" or changed since ({} of them)", l.changed)
                            } else {
                                String::new()
                            }
                        ),
                    )
                    .hint(hint),
                );
            }
        }
    } else if let Some(n) = &x.node {
        let jobs = counter(&n.counters, "local.UnknownDependency");
        if jobs > 0 {
            out.push(f(
                Level::Note,
                format!(
                    "{jobs} jobs stayed local because a dependency was unknown to crabfarm; run `crabfarm doctor` inside the project to see why."
                ),
            ));
        }
    }

    // Peers.
    if let Some(n) = &x.node {
        if n.peers.is_empty() {
            out.push(
                f(Level::Warn, "The node knows no peers, so nothing is built elsewhere.").hint(
                    "Run `crabfarm farm invite` here and `crabfarm farm join <code>` on another machine. A machine that \
                     created its own farm cannot see this one: `crabfarm farm leave` there, then join.",
                ),
            );
        } else if n.peers.iter().all(|(_, s)| s.is_none()) {
            out.push(
                f(Level::Problem, "No peer is online.").hint(format!(
                    "Start the node on the other machines. When it runs there and `crabfarm status` there does not list this \
                     machine either, check that they can open TCP port {port} to each other and that both are in the same farm."
                )),
            );
        }
        for (p, s) in &n.peers {
            let Some(st) = s.as_ref() else {
                continue;
            };
            if !st.accepting {
                out.push(f(
                    Level::Note,
                    format!(
                        "{} is online, but does not accept remote jobs (`accept_remote_jobs = false`).",
                        p.node_name
                    ),
                ));
                continue;
            }
            let reach = Reach::from_code(counter(
                &n.counters,
                &format!("reach.{}", p.node_id.to_hex()),
            ));
            match reach {
                Reach::Failing => out.push(
                    f(
                        Level::Problem,
                        format!(
                            "{} is online, but job connections fail in both directions (status gets through, jobs do not).",
                            p.node_name
                        ),
                    )
                    .hint(format!(
                        "Allow crabfarmd to accept TCP port {port} in the firewall of at least one of the two machines, \
                         or connect them through a network that routes both ways."
                    )),
                ),
                Reach::DialBack => out.push(f(
                    Level::Note,
                    format!(
                        "{} cannot be reached from here, so it connects back for every job (dial-back). That works; \
                         opening TCP port {port} towards {} in its firewall or router makes it direct.",
                        p.node_name, p.node_name
                    ),
                )),
                Reach::Direct => out.push(f(Level::Ok, format!("{} is online and takes jobs directly.", p.node_name))),
                Reach::Untried => out.push(f(
                    Level::Ok,
                    format!("{} is online (no job sent to it yet).", p.node_name),
                )),
            }
        }
    }
    if out.iter().any(|x| x.level >= Level::Warn) {
        let where_log = x
            .log_path
            .as_ref()
            .map(|p| format!(" ({})", p.display()))
            .unwrap_or_default();
        out.push(f(
            Level::Note,
            format!(
                "Node log{where_log}: restart the node with `CRABFARM_LOG=debug` for verbose connection and dependency traces."
            ),
        ));
    }
    out
}

// ------------------------------------------------------------------ gathering

fn first_on_path(name: &str) -> Option<PathBuf> {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
}

fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().join(".cargo")))
}

/// `<dir>/deps` folders of a target directory: `<profile>/deps` and
/// `<triple>/<profile>/deps`.
fn deps_dirs(target: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(target) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if p.join("deps").is_dir() {
            out.push(p.join("deps"));
        } else if let Ok(inner) = std::fs::read_dir(&p) {
            for i in inner.flatten() {
                if i.path().join("deps").is_dir() {
                    out.push(i.path().join("deps"));
                }
            }
        }
    }
    out
}

fn key(p: &Path) -> String {
    let s = p
        .to_string_lossy()
        .replace('/', std::path::MAIN_SEPARATOR_STR);
    if cfg!(windows) { s.to_lowercase() } else { s }
}

pub fn scan_libs(target: &Path, graph: &crate::depgraph::DepGraph) -> Libs {
    let known: std::collections::HashMap<String, &crate::proto::ClientPath> =
        graph.paths().map(|c| (key(&c.to_local()), c)).collect();
    let mut libs = Libs {
        dir: target.to_path_buf(),
        ..Libs::default()
    };
    for d in deps_dirs(target) {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let ext = p.extension().and_then(|x| x.to_str());
            let lib = ext.is_some_and(|x| x == "rlib" || x == "rmeta");
            if !lib || !p.is_file() {
                continue;
            }
            // `cargo clippy --all-targets` leaves 0-byte `.rmeta` placeholders
            // for `--test` targets (CF-44); nothing depends on them.
            if ext == Some("rmeta") && p.metadata().is_ok_and(|m| m.len() == 0) {
                continue;
            }
            libs.total += 1;
            let abs = std::path::absolute(&p).unwrap_or(p);
            match known.get(&key(&abs)).and_then(|c| graph.state_of(c)) {
                None => libs.unrecorded += 1,
                Some(false) => libs.changed += 1,
                Some(true) => {}
            }
        }
    }
    libs
}

/// `build.target-dir` of one cargo config file, relative paths resolved
/// against the folder that holds `.cargo` (cargo's config-relative rule).
fn target_dir_in(file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    let v = table
        .get("build")?
        .as_table()?
        .get("target-dir")?
        .as_str()?;
    let p = PathBuf::from(v);
    Some(if p.is_absolute() {
        p
    } else {
        file.parent()?.parent()?.join(p)
    })
}

/// The target directory cargo uses for `root` when run from `cwd`.
fn target_dir(root: &Path, cwd: &Path, cargo_home: Option<&Path>) -> PathBuf {
    for v in ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"] {
        if let Some(d) = std::env::var_os(v).filter(|d| !d.is_empty()) {
            let d = PathBuf::from(d);
            return if d.is_absolute() { d } else { cwd.join(d) };
        }
    }
    let mut dirs: Vec<PathBuf> = cwd.ancestors().map(|d| d.join(".cargo")).collect();
    dirs.extend(cargo_home.map(Path::to_path_buf));
    dirs.iter()
        .find_map(|d| {
            ["config.toml", "config"]
                .iter()
                .find_map(|f| target_dir_in(&d.join(f)))
        })
        .unwrap_or_else(|| root.join("target"))
}

pub fn gather(data_dir: &Path) -> anyhow::Result<Facts> {
    let exe = std::env::current_exe()?;
    let cfg = config::Config::load(data_dir).unwrap_or_default();
    let node = match cargo_cmd::query_status(data_dir) {
        Some(Message::FarmStatus {
            local,
            peers,
            counters,
        }) => Some(NodeView {
            local,
            peers,
            counters,
        }),
        _ => None,
    };
    let autostart =
        if service::autostart_covers_data_dir(std::env::var_os("CRABFARM_HOME").as_deref()) {
            service::current_target()
                .ok()
                .and_then(|t| service::status(&service::RealHost::new(t.data_dir.clone()), &t).ok())
        } else {
            None
        };
    let mut env_wrappers = Vec::new();
    for var in ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"] {
        if let Ok(v) = std::env::var(var)
            && !v.is_empty()
            && !cargo_cmd::is_crabfarm(&v, Some(&exe))
        {
            env_wrappers.push((var.to_string(), v));
        }
    }
    let cwd = std::env::current_dir()?;
    let project = crate::enable::find_project_root(&cwd).map(|root| {
        let home = cargo_home();
        let config_wrapper = cargo_cmd::effective_wrapper(&[], &cwd, home.as_deref(), &|_| None)
            .and_then(|(w, src)| match src {
                WrapperSource::File(file) => Some((w, file)),
                _ => None,
            });
        let graph = crate::depgraph::DepGraph::load(&data_dir.join("depgraph.bin"));
        let target = target_dir(&root, &cwd, home.as_deref());
        let libs = target.is_dir().then(|| scan_libs(&target, &graph));
        Project {
            root,
            config_wrapper,
            libs,
        }
    });
    Ok(Facts {
        in_farm: farm::has_identity(data_dir),
        created_farm: farm::holds_ca_key(data_dir),
        node_running: daemon::node_running(data_dir),
        node,
        unresponsive: ipc::unresponsive_until(data_dir).is_some(),
        autostart,
        on_path: first_on_path("crabfarm"),
        env_wrappers,
        project,
        listen_port: cfg.listen.rsplit(':').next().and_then(|p| p.parse().ok()),
        log_path: Some(data_dir.join("logs").join("node.log")),
        exe,
    })
}

// ------------------------------------------------------------------ command

fn apply(fix: Fix, data_dir: &Path, exe: &Path) -> anyhow::Result<String> {
    use service::Host;
    match fix {
        Fix::StartNode => {
            let daemon = service::daemon_path(exe)?;
            service::RealHost::new(data_dir.to_path_buf()).start_detached(&daemon)?;
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if cargo_cmd::query_status(data_dir).is_some() {
                    return Ok("started the node".into());
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            anyhow::bail!(
                "started crabfarmd, but it does not answer yet; see the log in {}",
                data_dir.join("logs").display()
            )
        }
        Fix::RepairAutostart => {
            let t = service::current_target()?;
            let mut host = service::RealHost::new(t.data_dir.clone());
            Ok(service::install(&mut host, &t)?.join(" "))
        }
        Fix::ClearUnresponsive => {
            ipc::clear_unresponsive(data_dir);
            Ok("wrappers use the node again".into())
        }
    }
}

fn label(l: Level) -> &'static str {
    match l {
        Level::Ok => "ok     ",
        Level::Note => "note   ",
        Level::Warn => "warning",
        Level::Problem => "PROBLEM",
    }
}

pub fn render(findings: &[Finding]) -> String {
    let mut out = String::new();
    for x in findings {
        out.push_str(&format!("{}  {}\n", label(x.level), x.text));
        if let Some(h) = &x.hint {
            out.push_str(&format!("         -> {h}\n"));
        }
    }
    out
}

pub fn run(fix: bool) -> anyhow::Result<i32> {
    let data_dir = config::data_dir()?;
    let facts = gather(&data_dir)?;
    println!(
        "crabfarm doctor {} ({})\n",
        env!("CARGO_PKG_VERSION"),
        facts.exe.display()
    );
    let findings = diagnose(&facts);
    print!("{}", render(&findings));
    let problems = findings
        .iter()
        .filter(|x| x.level == Level::Problem)
        .count();
    let warnings = findings.iter().filter(|x| x.level == Level::Warn).count();
    let fixable: Vec<Fix> = findings.iter().filter_map(|x| x.fix).collect();
    println!(
        "\n{problems} problem(s), {warnings} warning(s).{}",
        if fixable.is_empty() || fix {
            String::new()
        } else {
            format!(
                " `crabfarm doctor --fix` repairs {} of them here.",
                fixable.len()
            )
        }
    );
    if !fix || fixable.is_empty() {
        return Ok(i32::from(problems > 0));
    }
    let mut failed = false;
    for x in fixable {
        match apply(x, &data_dir, &facts.exe) {
            Ok(done) => println!("fixed: {done}"),
            Err(e) => {
                failed = true;
                println!("could not fix: {e:#}");
            }
        }
    }
    println!("Run `crabfarm doctor` again to check.");
    Ok(i32::from(failed))
}

// ------------------------------------------------------------ diagnostic dump

/// Default number of trailing lines of `node.log` included in `crabfarm doctor --dump`.
pub const DEFAULT_DUMP_LOG_LINES: usize = 200;
/// Maximum bytes read from the tail of `node.log` during `--dump`.
pub const MAX_DUMP_LOG_BYTES: usize = 256 * 1024;
/// Maximum `env_forward.toml` crates listed individually in `--dump`.
const MAX_DUMP_ENV_FORWARD_CRATES: usize = 32;

/// Non-secret cargo/crabfarm configuration variables inspected by `--dump`.
const DUMP_ENV_VARS: [&str; 8] = [
    "RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CRABFARM_INNER_WRAPPER",
    "CARGO_BUILD_JOBS",
    "CARGO_INCREMENTAL",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "CRABFARM_LOG",
];

#[derive(Debug, Clone, Default)]
pub struct CargoConfigSummary {
    pub file: PathBuf,
    pub rustc_wrapper: Option<String>,
    pub jobs: Option<String>,
}

#[derive(Debug)]
pub struct DiskSummary {
    pub short_farm_id: Option<String>,
    pub node_id_short: Option<String>,
    pub listen_addr: Option<String>,
    pub ipc_port: Option<u16>,
    pub unresponsive_remaining_secs: Option<u64>,
    pub peers_book: Result<Vec<PeerInfo>, String>,
    pub depgraph_entries: usize,
    pub ccache_usage: crate::ccache::DiskUsage,
    pub env_forward: std::collections::BTreeMap<String, Vec<String>>,
    pub auto_toolchains: Option<Vec<String>>,
}

#[derive(Debug, Default)]
pub struct LogTail {
    pub path: PathBuf,
    pub exists: bool,
    pub total_bytes: u64,
    pub lines: Vec<String>,
    pub truncated_by_bytes: bool,
}

pub struct DumpData {
    pub version: String,
    pub proto_version: u16,
    pub os: String,
    pub arch: String,
    pub now_unix: u64,
    pub data_dir: PathBuf,
    pub crabfarm_home_env: Option<String>,
    pub cwd: PathBuf,
    pub facts: Facts,
    pub config_source: String,
    pub config: config::Config,
    pub env_vars: Vec<(String, Option<String>)>,
    pub global_cargo_config: Option<CargoConfigSummary>,
    pub project_cargo_jobs: Option<(String, PathBuf)>,
    pub disk: DiskSummary,
    pub log_tail: LogTail,
}

fn read_cargo_config_summary(dir: &Path) -> Option<CargoConfigSummary> {
    for name in ["config.toml", "config"] {
        let file = dir.join(name);
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(table) = toml::from_str::<toml::Table>(&text) else {
            return Some(CargoConfigSummary {
                file,
                rustc_wrapper: Some("<invalid TOML>".into()),
                jobs: None,
            });
        };
        let build = table.get("build").and_then(|b| b.as_table());
        let rustc_wrapper = build
            .and_then(|b| b.get("rustc-wrapper"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string);
        let jobs = build.and_then(|b| b.get("jobs")).map(ToString::to_string);
        return Some(CargoConfigSummary {
            file,
            rustc_wrapper,
            jobs,
        });
    }
    None
}

fn project_jobs_in_ancestors(cwd: &Path, cargo_home: Option<&Path>) -> Option<(String, PathBuf)> {
    let mut dirs: Vec<PathBuf> = cwd.ancestors().map(|d| d.join(".cargo")).collect();
    dirs.extend(cargo_home.map(Path::to_path_buf));
    for d in dirs {
        if let Some(s) = read_cargo_config_summary(&d)
            && let Some(jobs) = s.jobs
        {
            return Some((jobs, s.file));
        }
    }
    None
}

/// Read only `farm/ca.crt` (public CA cert DER, never `ca.key`) to compute the short farm id.
fn short_farm_id_from_disk(data_dir: &Path) -> Option<String> {
    let der = std::fs::read(data_dir.join("farm").join("ca.crt")).ok()?;
    let fp = farm::cert_fingerprint(&der);
    Some(fp.iter().map(|b| format!("{b:02x}")).collect())
}

/// Read up to the last `max_lines` lines from `path`, bounded by `max_bytes` from EOF.
pub fn tail_log(path: &Path, max_lines: usize, max_bytes: usize) -> LogTail {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return LogTail {
            path: path.to_path_buf(),
            exists: false,
            ..LogTail::default()
        };
    };
    let total_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    if max_lines == 0 || total_bytes == 0 {
        return LogTail {
            path: path.to_path_buf(),
            exists: true,
            total_bytes,
            lines: Vec::new(),
            truncated_by_bytes: false,
        };
    }
    let cap = max_bytes.max(1) as u64;
    let (to_read, truncated_by_bytes) = if total_bytes > cap {
        let _ = file.seek(SeekFrom::End(-(cap as i64)));
        (cap as usize, true)
    } else {
        (total_bytes as usize, false)
    };
    let mut buf = Vec::with_capacity(to_read);
    let _ = file.take(to_read as u64).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let mut iter = text.lines();
    if truncated_by_bytes && buf.contains(&b'\n') {
        // Drop the leading partial line after seeking into the middle of the file,
        // unless the entire capped tail has no newline at all (single very long line).
        let _ = iter.next();
    }
    let all: Vec<String> = iter.map(sanitize_dump_line).collect();
    let skip = all.len().saturating_sub(max_lines);
    LogTail {
        path: path.to_path_buf(),
        exists: true,
        total_bytes,
        lines: all.into_iter().skip(skip).collect(),
        truncated_by_bytes,
    }
}

fn sanitize_dump_line(line: &str) -> String {
    line.chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .collect()
}

/// Strictly read-only diagnostic collector. Never reads keys (`ca.key`,
/// `node.key`, `blob.key`, `cache.key`), `ipc.token`, `invites.toml`, or
/// arbitrary environment variables.
pub fn gather_dump(data_dir: &Path, log_lines: usize) -> anyhow::Result<DumpData> {
    let now_unix = crate::node::unix_now();
    let facts = gather(data_dir)?;
    let cwd = std::env::current_dir()?;
    let home = cargo_home();

    let cfg_path = data_dir.join("config.toml");
    let (config_source, config) = match std::fs::read_to_string(&cfg_path) {
        Ok(s) => match toml::from_str::<config::Config>(&s) {
            Ok(c) => ("config.toml".into(), c),
            Err(e) => (
                format!("parse error ({e}); using defaults"),
                config::Config::default(),
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
            "default (config.toml absent)".into(),
            config::Config::default(),
        ),
        Err(e) => (
            format!("read error ({e}); using defaults"),
            config::Config::default(),
        ),
    };

    let env_vars = DUMP_ENV_VARS
        .iter()
        .map(|&name| (name.to_string(), std::env::var(name).ok()))
        .collect();

    let global_cargo_config = home.as_deref().and_then(read_cargo_config_summary);
    let project_cargo_jobs = project_jobs_in_ancestors(&cwd, home.as_deref());

    let node_id_short = std::fs::read_to_string(data_dir.join("node.id"))
        .ok()
        .map(|s| s.trim().chars().take(16).collect::<String>())
        .filter(|s| !s.is_empty());
    let listen_addr = std::fs::read_to_string(data_dir.join("listen.addr"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let ipc_port = std::fs::read_to_string(ipc::port_path(data_dir))
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok());
    let unresponsive_remaining_secs =
        ipc::unresponsive_until(data_dir).and_then(|u| u.checked_sub(now_unix));
    let peers_book = crate::discovery::PeerBook::load(data_dir)
        .map(|b| b.peers)
        .map_err(|e| format!("{e:#}"));
    let depgraph_entries = crate::depgraph::DepGraph::load(&data_dir.join("depgraph.bin")).len();
    let ccache_usage = crate::ccache::disk_usage(data_dir);
    let env_forward = crate::envlearn::summary(data_dir);

    #[derive(serde::Deserialize, Default)]
    struct AutoToolchainsFile {
        #[serde(default)]
        names: Vec<String>,
    }
    let auto_toolchains = std::fs::read_to_string(data_dir.join("auto-toolchains.toml"))
        .ok()
        .and_then(|s| toml::from_str::<AutoToolchainsFile>(&s).ok())
        .map(|f| f.names);

    let log_tail = tail_log(
        &data_dir.join("logs").join("node.log"),
        log_lines,
        MAX_DUMP_LOG_BYTES,
    );

    Ok(DumpData {
        version: env!("CARGO_PKG_VERSION").to_string(),
        proto_version: crate::proto::PROTO_VERSION,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        now_unix,
        data_dir: data_dir.to_path_buf(),
        crabfarm_home_env: std::env::var("CRABFARM_HOME").ok(),
        cwd,
        facts,
        config_source,
        config,
        env_vars,
        global_cargo_config,
        project_cargo_jobs,
        disk: DiskSummary {
            short_farm_id: short_farm_id_from_disk(data_dir),
            node_id_short,
            listen_addr,
            ipc_port,
            unresponsive_remaining_secs,
            peers_book,
            depgraph_entries,
            ccache_usage,
            env_forward,
            auto_toolchains,
        },
        log_tail,
    })
}

fn format_toolchain(tc: &crate::proto::ToolchainId, targets: Option<&[String]>) -> String {
    let short_hash: String = tc.commit_hash.chars().take(10).collect();
    let t_list = targets
        .map(|ts| ts.join(", "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".into());
    format!(
        "{} (commit={}, date={}, host={}) targets=[{}]",
        tc.release, short_hash, tc.commit_date, tc.host, t_list
    )
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Render [`DumpData`] into a self-contained, secret-free plain-text report.
pub fn render_dump(d: &DumpData) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let _ = writeln!(
        out,
        "=== crabfarm diagnostic dump ({}, proto {}) ===",
        d.version, d.proto_version
    );
    let _ = writeln!(out, "platform: {}/{}", d.os, d.arch);
    let _ = writeln!(out, "unix_time: {}", d.now_unix);
    let _ = writeln!(out, "exe: {}", d.facts.exe.display());
    let _ = writeln!(
        out,
        "on_path: {}",
        d.facts
            .on_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "not found".into())
    );
    let _ = writeln!(
        out,
        "data_dir: {} (CRABFARM_HOME={})",
        d.data_dir.display(),
        d.crabfarm_home_env.as_deref().unwrap_or("unset")
    );
    let _ = writeln!(out, "cwd: {}", d.cwd.display());
    if let Some(a) = &d.facts.autostart {
        let _ = writeln!(
            out,
            "autostart: {} (stale={}, node_running={})",
            if a.autostart { "on" } else { "off" },
            a.stale,
            a.node_running
        );
    } else {
        let _ = writeln!(out, "autostart: n/a");
    }

    // 1. Doctor findings
    let findings = diagnose(&d.facts);
    let problems = findings
        .iter()
        .filter(|x| x.level == Level::Problem)
        .count();
    let warnings = findings.iter().filter(|x| x.level == Level::Warn).count();
    let _ = writeln!(
        out,
        "\n--- doctor findings ({problems} problem(s), {warnings} warning(s)) ---"
    );
    out.push_str(&render(&findings));

    // 2. Configuration
    let c = &d.config;
    let _ = writeln!(out, "\n--- config ({}) ---", d.config_source);
    let _ = writeln!(out, "node_name = {:?}", c.node_name);
    let _ = writeln!(out, "listen = {:?}", c.listen);
    let _ = writeln!(
        out,
        "slots = {}  local_slots = {}  accept_remote_jobs = {}  prefer_remote = {}",
        c.slots, c.local_slots, c.accept_remote_jobs, c.prefer_remote
    );
    let _ = writeln!(
        out,
        "remote_incremental = {:?}  auto_install = {} (max={})  mdns = {}",
        c.remote_incremental, c.auto_install, c.max_auto_toolchains, c.mdns
    );
    let _ = writeln!(
        out,
        "cache = {} (entries_max={} GB, blobs_max={} GB, retain_blobs={})  job_timeout_min = {}  accept_build_env = {}",
        c.cache,
        c.cache_entries_max_gb,
        c.cache_max_gb,
        c.retain_blobs,
        c.job_timeout_min,
        c.accept_build_env
    );
    let _ = writeln!(out, "peers = {:?}", c.peers);
    let _ = writeln!(out, "local_only = {:?}", c.local_only);
    let _ = writeln!(out, "sensitive = {:?}", c.sensitive);
    let _ = writeln!(out, "forward_env (names) = {:?}", c.forward_env);
    let _ = writeln!(out, "accept_env (names) = {:?}", c.accept_env);

    // 3. Cargo & wrapper setup
    let _ = writeln!(out, "\n--- cargo & wrapper setup ---");
    for (k, v) in &d.env_vars {
        match v {
            Some(val) => {
                let _ = writeln!(out, "env {k} = {}", sanitize_dump_line(val));
            }
            None => {
                let _ = writeln!(out, "env {k} = <unset>");
            }
        }
    }
    match &d.global_cargo_config {
        Some(g) => {
            let _ = writeln!(
                out,
                "global cargo config ({}): rustc-wrapper={}, jobs={}",
                g.file.display(),
                g.rustc_wrapper.as_deref().unwrap_or("<unset>"),
                g.jobs.as_deref().unwrap_or("<unset>")
            );
        }
        None => {
            let _ = writeln!(out, "global cargo config: none found");
        }
    }
    match &d.facts.project {
        Some(p) => {
            let _ = writeln!(out, "project root: {}", p.root.display());
            match &p.config_wrapper {
                Some((w, file)) => {
                    let _ = writeln!(
                        out,
                        "project effective rustc-wrapper: {w:?} ({})",
                        file.display()
                    );
                }
                None => {
                    let _ = writeln!(out, "project effective rustc-wrapper: <none>");
                }
            }
            if let Some((jobs, file)) = &d.project_cargo_jobs {
                let _ = writeln!(
                    out,
                    "project effective build.jobs: {jobs} ({})",
                    file.display()
                );
            }
            if let Some(l) = &p.libs {
                let _ = writeln!(
                    out,
                    "project libs ({}): total={}, unrecorded={}, changed={}",
                    l.dir.display(),
                    l.total,
                    l.unrecorded,
                    l.changed
                );
            }
        }
        None => {
            let _ = writeln!(
                out,
                "project: cwd is not inside a Rust project (run inside your project dir to inspect its .cargo/config.toml and target/deps)"
            );
        }
    }

    // 4. Live node & farm state
    let _ = writeln!(out, "\n--- live node & farm state ---");
    match &d.facts.node {
        Some(n) => {
            out.push_str(&crate::status::format_status(
                &n.local,
                &n.peers,
                &n.counters,
                d.now_unix,
            ));
            let _ = writeln!(
                out,
                "\nlocal node: id={} name={} host={} accepting={} slots={}/{} speed={}",
                &n.local.node_id.to_hex()[..16],
                n.local.node_name,
                n.local.host_triple,
                n.local.accepting,
                n.local.slots_busy,
                n.local.slots_total,
                n.local.speed
            );
            let _ = writeln!(out, "local toolchains ({}):", n.local.toolchains.len());
            for (i, tc) in n.local.toolchains.iter().enumerate() {
                let targets = n.local.targets.get(i).map(Vec::as_slice);
                let _ = writeln!(out, "  - {}", format_toolchain(tc, targets));
            }
            if n.peers.is_empty() {
                let _ = writeln!(out, "peers: none");
            } else {
                for (p, s) in &n.peers {
                    let reach = Reach::from_code(counter(
                        &n.counters,
                        &format!("reach.{}", p.node_id.to_hex()),
                    ));
                    match s {
                        Some(st) => {
                            let _ = writeln!(
                                out,
                                "peer {} (id={}): ONLINE host={} accepting={} slots={}/{} speed={} reach={:?} addrs={:?}",
                                crate::proto::log_safe(&p.node_name),
                                &p.node_id.to_hex()[..16],
                                crate::proto::log_safe(&st.host_triple),
                                st.accepting,
                                st.slots_busy,
                                st.slots_total,
                                st.speed,
                                reach,
                                p.addrs
                            );
                            let _ = writeln!(out, "  toolchains ({}):", st.toolchains.len());
                            for (i, tc) in st.toolchains.iter().enumerate() {
                                let targets = st.targets.get(i).map(Vec::as_slice);
                                let _ = writeln!(out, "    - {}", format_toolchain(tc, targets));
                            }
                            if !st.accepting {
                                let _ = writeln!(
                                    out,
                                    "  NOTE: peer has accept_remote_jobs = false; no jobs will be sent to it"
                                );
                            } else if !n.local.toolchains.is_empty() {
                                let matched = n
                                    .local
                                    .toolchains
                                    .iter()
                                    .filter(|lt| {
                                        st.toolchains.iter().zip(&st.targets).any(|(pt, pts)| {
                                            pt.same_compiler(lt)
                                                && pts.iter().any(|t| t == &n.local.host_triple)
                                        })
                                    })
                                    .count();
                                if matched == 0 {
                                    let _ = writeln!(
                                        out,
                                        "  NOTE: 0 of {} local toolchain(s) match this peer with target {} (check rustc version/commit or auto_install)",
                                        n.local.toolchains.len(),
                                        n.local.host_triple
                                    );
                                } else {
                                    let _ = writeln!(
                                        out,
                                        "  toolchain match: {matched}/{} local toolchain(s) ready for {}",
                                        n.local.toolchains.len(),
                                        n.local.host_triple
                                    );
                                }
                            }
                        }
                        None => {
                            let _ = writeln!(
                                out,
                                "peer {} (id={}): OFFLINE last_seen={} addrs={:?}",
                                crate::proto::log_safe(&p.node_name),
                                &p.node_id.to_hex()[..16],
                                p.last_seen,
                                p.addrs
                            );
                        }
                    }
                }
            }
            if !c.prefer_remote {
                let _ = writeln!(
                    out,
                    "scheduling note: prefer_remote = false (local_slots={}); when local free slots >= peer free slots, ties run locally (LocalSlotFree)",
                    c.local_slots
                );
            }
            let mut sorted_counters = n.counters.clone();
            sorted_counters.sort_by(|a, b| a.0.cmp(&b.0));
            let _ = writeln!(out, "\ncounters ({}):", sorted_counters.len());
            for (k, v) in sorted_counters {
                let _ = writeln!(out, "  {k} = {v}");
            }
        }
        None => {
            let _ = writeln!(
                out,
                "node did not answer QueryStatus (node_running={})",
                d.facts.node_running
            );
        }
    }

    // 5. On-disk state summary (no keys/tokens)
    let _ = writeln!(out, "\n--- on-disk state (no keys or tokens) ---");
    let _ = writeln!(
        out,
        "farm: in_farm={} created_farm={} short_farm_id={} node_id={}",
        d.facts.in_farm,
        d.facts.created_farm,
        d.disk.short_farm_id.as_deref().unwrap_or("<none>"),
        d.disk.node_id_short.as_deref().unwrap_or("<none>")
    );
    let _ = writeln!(
        out,
        "node.lock: {}  listen.addr: {}  ipc.port: {}",
        if d.facts.node_running {
            "held (running)"
        } else {
            "free"
        },
        d.disk.listen_addr.as_deref().unwrap_or("<absent>"),
        d.disk
            .ipc_port
            .map(|p| p.to_string())
            .unwrap_or_else(|| "<absent>".into())
    );
    let _ = writeln!(
        out,
        "node-unresponsive: {}",
        d.disk
            .unresponsive_remaining_secs
            .map(|s| format!("ACTIVE ({s}s left)"))
            .unwrap_or_else(|| "absent".into())
    );
    match &d.disk.peers_book {
        Ok(peers) => {
            let _ = writeln!(out, "peers.toml: {} peer(s)", peers.len());
            for p in peers {
                let _ = writeln!(
                    out,
                    "  - {} (id={}) addrs={:?} last_seen={}",
                    crate::proto::log_safe(&p.node_name),
                    &p.node_id.to_hex()[..16],
                    p.addrs,
                    p.last_seen
                );
            }
        }
        Err(e) => {
            let _ = writeln!(out, "peers.toml: error ({e})");
        }
    }
    let _ = writeln!(
        out,
        "depgraph.bin: {} recorded entries",
        d.disk.depgraph_entries
    );
    let _ = writeln!(
        out,
        "ccache: {} entries, {} blobs, {}",
        d.disk.ccache_usage.entries,
        d.disk.ccache_usage.blobs,
        format_bytes(d.disk.ccache_usage.bytes)
    );
    let _ = writeln!(
        out,
        "env_forward.toml: {} crate(s) with recorded local env deps (names only)",
        d.disk.env_forward.len()
    );
    for (krate, vars) in d.disk.env_forward.iter().take(MAX_DUMP_ENV_FORWARD_CRATES) {
        let _ = writeln!(out, "  - {krate}: {:?}", vars);
    }
    if d.disk.env_forward.len() > MAX_DUMP_ENV_FORWARD_CRATES {
        let _ = writeln!(
            out,
            "  ... ({} more crates omitted)",
            d.disk.env_forward.len() - MAX_DUMP_ENV_FORWARD_CRATES
        );
    }
    if let Some(auto) = &d.disk.auto_toolchains {
        let _ = writeln!(out, "auto-toolchains.toml: {:?}", auto);
    }

    // 6. Recent node.log tail
    if !d.log_tail.exists {
        let _ = writeln!(
            out,
            "\n--- node.log ({}) ---\n<file does not exist>",
            d.log_tail.path.display()
        );
    } else {
        let _ = writeln!(
            out,
            "\n--- node.log ({}, {}, showing last {} line(s){}) ---",
            d.log_tail.path.display(),
            format_bytes(d.log_tail.total_bytes),
            d.log_tail.lines.len(),
            if d.log_tail.truncated_by_bytes {
                ", tail capped at 256 KiB"
            } else {
                ""
            }
        );
        for line in &d.log_tail.lines {
            out.push_str(line);
            out.push('\n');
        }
    }

    out
}

/// Run `crabfarm doctor --dump` (write to `out_path` or stdout).
pub fn run_dump(out_path: Option<&Path>, log_lines: usize) -> anyhow::Result<i32> {
    let data_dir = config::data_dir()?;
    let data = gather_dump(&data_dir, log_lines)?;
    let report = render_dump(&data);
    match out_path {
        Some(path) => {
            std::fs::write(path, &report)
                .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))?;
            println!(
                "Wrote diagnostic dump ({}) to {}",
                format_bytes(report.len() as u64),
                path.display()
            );
        }
        None => {
            print!("{report}");
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::NodeId;

    fn st(name: &str) -> Status {
        Status {
            node_id: NodeId([1; 16]),
            node_name: name.into(),
            host_triple: "x86_64-pc-windows-msvc".into(),
            slots_total: 16,
            slots_busy: 0,
            queue_len: 0,
            speed: 100,
            toolchains: vec![],
            targets: vec![],
            accepting: true,
        }
    }

    fn healthy() -> Facts {
        let exe = std::env::current_exe().unwrap();
        Facts {
            on_path: Some(exe.clone()),
            exe,
            in_farm: true,
            node_running: true,
            node: Some(NodeView {
                local: st("me"),
                peers: vec![(
                    PeerInfo {
                        node_id: NodeId([2; 16]),
                        node_name: "desktop".into(),
                        addrs: vec![],
                        last_seen: 1,
                    },
                    Some(st("desktop")),
                )],
                counters: vec![(
                    format!("reach.{}", NodeId([2; 16]).to_hex()),
                    Reach::Direct.code(),
                )],
            }),
            autostart: Some(service::Status {
                autostart: true,
                stale: false,
                node_running: true,
            }),
            ..Facts::default()
        }
    }

    fn worst(f: &[Finding]) -> Level {
        f.iter().map(|x| x.level).max().unwrap()
    }

    #[test]
    fn a_healthy_machine_has_nothing_to_report() {
        let f = diagnose(&healthy());
        assert_eq!(worst(&f), Level::Ok, "{}", render(&f));
        assert!(render(&f).contains("desktop is online and takes jobs directly"));
    }

    #[test]
    fn a_wrapper_in_the_environment_is_named_with_the_way_out() {
        let mut x = healthy();
        x.env_wrappers = vec![("RUSTC_WRAPPER".into(), "sccache".into())];
        let f = diagnose(&x);
        let r = render(&f);
        assert_eq!(worst(&f), Level::Problem, "{r}");
        assert!(
            r.contains("RUSTC_WRAPPER=sccache") && r.contains("UnknownDependency"),
            "{r}"
        );
        assert!(
            r.contains("CRABFARM_INNER_WRAPPER=sccache") && r.contains("crabfarm cargo build"),
            "{r}"
        );
    }

    #[test]
    fn a_stopped_node_is_started_by_fix_only_in_a_farm() {
        let mut x = healthy();
        x.node = None;
        x.node_running = false;
        let f = diagnose(&x);
        assert!(
            f.iter().any(|y| y.fix == Some(Fix::StartNode)),
            "{}",
            render(&f)
        );
        x.in_farm = false;
        let f = diagnose(&x);
        assert!(
            f.iter().all(|y| y.fix != Some(Fix::StartNode)),
            "{}",
            render(&f)
        );
        assert!(render(&f).contains("in no farm"));
    }

    #[test]
    fn reachability_is_explained_per_peer() {
        let mut x = healthy();
        let id = format!("reach.{}", NodeId([2; 16]).to_hex());
        x.node.as_mut().unwrap().counters = vec![(id.clone(), Reach::DialBack.code())];
        let r = render(&diagnose(&x));
        assert!(r.contains("connects back for every job (dial-back)"), "{r}");
        x.node.as_mut().unwrap().counters = vec![(id, Reach::Failing.code())];
        let f = diagnose(&x);
        assert_eq!(worst(&f), Level::Problem);
        assert!(render(&f).contains("fail in both directions"));
    }

    #[test]
    fn unknown_libraries_in_the_project_point_at_a_clean_build() {
        let mut x = healthy();
        x.node
            .as_mut()
            .unwrap()
            .counters
            .push(("local.UnknownDependency".into(), 50));
        x.project = Some(Project {
            root: PathBuf::from("proj"),
            config_wrapper: None,
            libs: Some(Libs {
                dir: PathBuf::from("proj/target"),
                total: 300,
                unrecorded: 120,
                changed: 3,
            }),
        });
        let f = diagnose(&x);
        let r = render(&f);
        assert!(
            r.contains("123 of 300 libraries") && r.contains("cargo clean"),
            "{r}"
        );
        assert_eq!(worst(&f), Level::Warn, "{r}");
    }

    #[test]
    fn the_library_scan_tells_recorded_changed_and_unknown_apart() {
        let d = tempfile::tempdir().unwrap();
        let deps = d.path().join("target/debug/deps");
        let cross = d
            .path()
            .join("target/x86_64-unknown-linux-gnu/release/deps");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::create_dir_all(&cross).unwrap();
        let rec = deps.join("liba-1.rmeta");
        let chg = deps.join("libb-2.rlib");
        let unk = cross.join("libc-3.rlib");
        for p in [&rec, &chg, &unk] {
            std::fs::write(p, b"x").unwrap();
        }
        std::fs::write(deps.join("a-1.d"), b"not a library").unwrap();
        let mut g = crate::depgraph::DepGraph::default();
        let cp = |p: &Path| crate::proto::ClientPath::from_local(p, Path::new(".")).unwrap();
        g.record(&[cp(&rec), cp(&chg)], &[]);
        std::fs::write(&chg, b"rebuilt elsewhere").unwrap();
        let l = scan_libs(&d.path().join("target"), &g);
        assert_eq!((l.total, l.unrecorded, l.changed), (3, 1, 1), "{l:?}");
    }

    /// CF-44: `cargo clippy --all-targets` writes 0-byte `.rmeta` files for
    /// `--test` targets; `scan_libs` must not count them as real libraries.
    #[test]
    fn zero_byte_rmeta_files_from_clippy_tests_are_ignored() {
        let d = tempfile::tempdir().unwrap();
        let deps = d.path().join("target/debug/deps");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(deps.join("libtest_bin-1.rmeta"), b"").unwrap();
        let g = crate::depgraph::DepGraph::default();
        let l = scan_libs(&d.path().join("target"), &g);
        assert_eq!((l.total, l.unrecorded, l.changed), (0, 0, 0), "{l:?}");
    }

    /// CF-45: when doctor finds a warning or problem, it tells the user where
    /// the log lives and how to enable debug logs.
    #[test]
    fn doctor_shows_debug_log_hint_on_warning_or_problem() {
        let mut x = healthy();
        x.node = None;
        x.node_running = false;
        let r = render(&diagnose(&x));
        assert!(
            r.contains("CRABFARM_LOG=debug"),
            "missing debug log hint:\n{r}"
        );
    }

    #[test]
    fn non_accepting_peer_is_noted_by_doctor() {
        let mut x = healthy();
        x.node.as_mut().unwrap().peers[0]
            .1
            .as_mut()
            .unwrap()
            .accepting = false;
        let f = diagnose(&x);
        let r = render(&f);
        assert!(
            r.contains("does not accept remote jobs"),
            "missing non-accepting note:\n{r}"
        );
        assert_eq!(worst(&f), Level::Note);
    }

    #[test]
    fn tail_log_handles_missing_small_and_large_files() {
        let d = tempfile::tempdir().unwrap();
        let missing = tail_log(&d.path().join("nope.log"), 50, 1024);
        assert!(!missing.exists);
        assert!(missing.lines.is_empty());

        let log = d.path().join("node.log");
        std::fs::write(&log, "line1\nline2\nline3\nline4\n").unwrap();
        let small = tail_log(&log, 2, 1024);
        assert!(small.exists);
        assert!(!small.truncated_by_bytes);
        assert_eq!(small.lines, vec!["line3", "line4"]);

        // Large file exceeding byte cap: seeks from tail and drops the partial leading line.
        let mut big = String::new();
        for i in 0..200 {
            big.push_str(&format!("entry-{i:04}-payload-0123456789\n"));
        }
        std::fs::write(&log, &big).unwrap();
        let capped = tail_log(&log, 500, 512);
        assert!(capped.truncated_by_bytes);
        assert_eq!(
            capped.lines.last().unwrap(),
            "entry-0199-payload-0123456789"
        );
        assert!(
            capped.lines.first().unwrap().starts_with("entry-"),
            "partial leading line should be dropped: {:?}",
            capped.lines.first()
        );

        // Single line longer than the byte cap is still preserved instead of yielding empty.
        std::fs::write(&log, "x".repeat(1024)).unwrap();
        let one_long = tail_log(&log, 10, 256);
        assert!(one_long.truncated_by_bytes);
        assert_eq!(one_long.lines.len(), 1);
        assert_eq!(one_long.lines[0].len(), 256);
    }

    #[test]
    fn diagnostic_dump_includes_toolchains_config_log_and_excludes_secrets() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("farm")).unwrap();
        std::fs::create_dir_all(d.path().join("logs")).unwrap();
        // Plant recognizable secret values in every secret file that --dump must NEVER read.
        std::fs::write(d.path().join("farm/ca.key"), "SECRET_CA_KEY_VALUE").unwrap();
        std::fs::write(d.path().join("node.key"), "SECRET_NODE_KEY_VALUE").unwrap();
        std::fs::write(d.path().join("blob.key"), "SECRET_BLOB_KEY_VALUE").unwrap();
        std::fs::write(d.path().join("cache.key"), "SECRET_CACHE_KEY_VALUE").unwrap();
        std::fs::write(d.path().join("ipc.token"), "SECRET_IPC_TOKEN_VALUE").unwrap();
        std::fs::write(d.path().join("farm/invites.toml"), "SECRET_INVITE_TOKEN").unwrap();
        std::fs::write(d.path().join("farm/ca.crt"), b"dummy-ca-cert-der").unwrap();
        std::fs::write(d.path().join("node.id"), "0123456789abcdef0123456789abcdef").unwrap();
        std::fs::write(
            d.path().join("config.toml"),
            "prefer_remote = true\nlocal_slots = 4\nforward_env = [\"MY_SAFE_VAR\"]\n",
        )
        .unwrap();
        std::fs::write(
            d.path().join("env_forward.toml"),
            "my_crate = [\"UNFORWARDED_VAR\"]\n",
        )
        .unwrap();
        std::fs::write(
            d.path().join("logs/node.log"),
            "2026-11-23 INFO node started\n2026-11-23 INFO job fell back\n",
        )
        .unwrap();

        let mut dump = gather_dump(d.path(), 50).unwrap();
        // Inject a live NodeView with mismatched toolchains to verify compatibility reporting.
        let mut local_st = st("FOCALOR");
        local_st.toolchains = vec![crate::proto::ToolchainId {
            release: "1.100.0".into(),
            commit_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            commit_date: "2026-11-09".into(),
            host: "x86_64-pc-windows-msvc".into(),
        }];
        local_st.targets = vec![vec!["x86_64-pc-windows-msvc".into()]];

        let mut peer_st = st("DESKTOP-PC");
        peer_st.toolchains = vec![crate::proto::ToolchainId {
            release: "1.99.0".into(),
            commit_hash: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            commit_date: "2026-10-01".into(),
            host: "x86_64-pc-windows-msvc".into(),
        }];
        peer_st.targets = vec![vec!["x86_64-pc-windows-msvc".into()]];

        dump.facts.node = Some(NodeView {
            local: local_st,
            peers: vec![(
                PeerInfo {
                    node_id: NodeId([2; 16]),
                    node_name: "DESKTOP-PC".into(),
                    addrs: vec!["192.168.1.20:47070".into()],
                    last_seen: 1000,
                },
                Some(peer_st),
            )],
            counters: vec![("local.NoPeer".into(), 7), ("remote_ok".into(), 0)],
        });

        let text = render_dump(&dump);
        assert!(text.contains("=== crabfarm diagnostic dump"), "{text}");
        assert!(text.contains("prefer_remote = true"), "{text}");
        assert!(text.contains("MY_SAFE_VAR"), "{text}");
        assert!(text.contains("UNFORWARDED_VAR"), "{text}");
        assert!(text.contains("0 of 1 local toolchain(s) match"), "{text}");
        assert!(text.contains("local.NoPeer = 7"), "{text}");
        assert!(text.contains("INFO job fell back"), "{text}");
        for forbidden in [
            "SECRET_CA_KEY_VALUE",
            "SECRET_NODE_KEY_VALUE",
            "SECRET_BLOB_KEY_VALUE",
            "SECRET_CACHE_KEY_VALUE",
            "SECRET_IPC_TOKEN_VALUE",
            "SECRET_INVITE_TOKEN",
        ] {
            assert!(!text.contains(forbidden), "dump leaked secret {forbidden}!");
        }
    }
}
