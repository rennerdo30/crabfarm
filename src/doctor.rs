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
            if s.is_none() {
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
}
