//! CF-28 end to end: the compile cache (DESIGN section 16). Real nodes (child
//! processes of the built binary, loopback only, mDNS off) build `tests/fixture`
//! through `crabfarm cargo` and the cache serves what a build already made.
//!
//! Safety (owner PC): everything binds 127.0.0.1 only; every child has a
//! timeout; only the PIDs started here are killed. The tests run one at a
//! time (a shared lock), so the machine never carries more than one farm.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_crabfarm");

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn spawn_tree(cmd: &mut Command) -> Child {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().expect("spawn")
}

fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        // SAFETY: signals the process group this test created for `child`.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

struct Proc {
    child: Child,
}

impl Drop for Proc {
    fn drop(&mut self) {
        kill_tree(&mut self.child);
    }
}

struct Output {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn crabfarm(home: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.env("CRABFARM_HOME", home);
    for v in [
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_TARGET_DIR",
        "CRABFARM_LOG",
    ] {
        c.env_remove(v);
    }
    c
}

fn run(mut cmd: Command, secs: u64) -> Output {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_tree(&mut cmd);
    let mut out = child.stdout.take().expect("stdout");
    let mut err = child.stderr.take().expect("stderr");
    let t_out = std::thread::spawn(move || {
        let mut s = Vec::new();
        std::io::Read::read_to_end(&mut out, &mut s).ok();
        s
    });
    let t_err = std::thread::spawn(move || {
        let mut s = Vec::new();
        std::io::Read::read_to_end(&mut err, &mut s).ok();
        s
    });
    let deadline = Instant::now() + Duration::from_secs(secs);
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s);
        }
        if Instant::now() >= deadline {
            kill_tree(&mut child);
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = String::from_utf8_lossy(&t_out.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&t_err.join().unwrap_or_default()).into_owned();
    assert!(
        status.is_some(),
        "command timed out after {secs}s\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    Output {
        ok: status.is_some_and(|s| s.success()),
        stdout,
        stderr,
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

fn node_log(home: &Path) -> String {
    let text = std::fs::read_to_string(home.join("logs").join("node.log")).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(40)..].join("\n")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for e in std::fs::read_dir(from).expect("read_dir") {
        let e = e.expect("entry");
        let dest = to.join(e.file_name());
        if e.file_type().expect("type").is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), &dest).expect("copy");
        }
    }
}

/// A node's data directory, port and the process once started.
struct Node {
    home: PathBuf,
    port: u16,
    _proc: Option<Proc>,
}

fn write_config(home: &Path, name: &str, port: u16, peer_port: u16, extra: &str) {
    std::fs::create_dir_all(home).expect("home");
    let text = format!(
        "node_name = \"{name}\"\nlisten = \"127.0.0.1:{port}\"\npeers = [\"127.0.0.1:{peer_port}\"]\nmdns = false\nslots = 4\nauto_install = false\n{extra}\n"
    );
    std::fs::write(home.join("config.toml"), text).expect("config");
}

fn start_node(home: &Path) -> Proc {
    let mut c = crabfarm(home);
    c.arg("node")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let p = Proc {
        child: c.spawn().expect("spawn node"),
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while !home.join("ipc.port").exists() {
        assert!(
            Instant::now() < deadline,
            "node did not start; log:\n{}",
            node_log(home)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    p
}

fn status(home: &Path) -> String {
    let mut c = crabfarm(home);
    c.arg("status");
    run(c, 20).stdout
}

/// One node with a farm of its own (the peer entry points at a closed port).
fn single_node(root: &Path, extra: &str) -> Node {
    let home = root.join("home1");
    let (port, dead) = (free_port(), free_port());
    write_config(&home, "node-one", port, dead, extra);
    let mut c = crabfarm(&home);
    c.args(["farm", "create", "--name", "node-one"]);
    assert!(run(c, 60).ok, "farm create failed");
    let proc = start_node(&home);
    Node {
        home,
        port,
        _proc: Some(proc),
    }
}

/// Two nodes in one farm; `extra1` and `extra2` are their config lines.
fn two_nodes(root: &Path, extra1: &str, extra2: &str) -> (Node, Node) {
    let (home1, home2) = (root.join("home1"), root.join("home2"));
    let (p1, p2) = (free_port(), free_port());
    write_config(&home1, "node-one", p1, p2, extra1);
    write_config(&home2, "node-two", p2, p1, extra2);
    let mut c = crabfarm(&home1);
    c.args(["farm", "create", "--name", "node-one"]);
    assert!(run(c, 60).ok, "farm create failed");
    let proc1 = start_node(&home1);
    let mut c = crabfarm(&home1);
    c.args(["farm", "invite", "--addr"])
        .arg(format!("127.0.0.1:{p1}"));
    let invite = run(c, 30);
    assert!(invite.ok, "farm invite failed: {}", invite.stderr);
    let mut c = crabfarm(&home2);
    c.args(["farm", "join", invite.stdout.trim(), "--name", "node-two"]);
    let join = run(c, 60);
    assert!(join.ok, "farm join failed: {}", join.stderr);
    let proc2 = start_node(&home2);
    for home in [&home1, &home2] {
        let deadline = Instant::now() + Duration::from_secs(90);
        while !status(home).contains("1 of 1 peers online") {
            assert!(
                Instant::now() < deadline,
                "peers never connected\nnode one log:\n{}\nnode two log:\n{}",
                node_log(&home1),
                node_log(&home2)
            );
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    (
        Node {
            home: home1,
            port: p1,
            _proc: Some(proc1),
        },
        Node {
            home: home2,
            port: p2,
            _proc: Some(proc2),
        },
    )
}

fn cargo_build(home: &Path, work: &Path, target: &Path) -> Output {
    let mut c = crabfarm(home);
    c.args([
        "cargo",
        "build",
        "--release",
        "--color",
        "never",
        "--manifest-path",
    ])
    .arg(work.join("Cargo.toml"))
    .env("CARGO_TARGET_DIR", target)
    .env("CARGO_NET_OFFLINE", "true");
    run(c, 150)
}

fn build_ok(n: &Node, work: &Path, target: &Path) -> Output {
    let b = cargo_build(&n.home, work, target);
    assert!(
        b.ok,
        "build failed\nstdout:\n{}\nstderr:\n{}\nlog:\n{}",
        b.stdout,
        b.stderr,
        node_log(&n.home)
    );
    b
}

/// What `crabfarm status` says about the compile cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cache {
    local: u64,
    farm: u64,
    misses: u64,
    stored: u64,
    served: u64,
    unusable: u64,
}

fn cache(home: &Path) -> Cache {
    let st = status(home);
    let line = st
        .lines()
        .find(|l| l.starts_with("compile cache:"))
        .unwrap_or_else(|| panic!("no compile cache line in status:\n{st}"));
    let tokens: Vec<&str> = line
        .split(|c: char| c.is_whitespace() || "(),:".contains(c))
        .filter(|t| !t.is_empty())
        .collect();
    let before = |word: &str| -> u64 {
        let i = tokens
            .iter()
            .position(|t| *t == word)
            .unwrap_or_else(|| panic!("no `{word}` in: {line}"));
        tokens[i - 1].parse().unwrap_or_else(|_| panic!("{line}"))
    };
    Cache {
        local: before("local"),
        farm: before("farm"),
        misses: before("misses"),
        stored: before("stored"),
        served: before("served"),
        unusable: before("unusable"),
    }
}

fn remote_ok(home: &Path) -> u64 {
    let st = status(home);
    st.lines()
        .find_map(|l| {
            let rest = l.strip_prefix("jobs: ")?;
            rest.split(',')
                .find_map(|p| p.trim().strip_suffix(" remote ok")?.parse().ok())
        })
        .unwrap_or_else(|| panic!("no jobs line in status:\n{st}"))
}

/// Hash of every `.rlib`, `.rmeta` and `.d` the build left in the target
/// directory (the unit directories differ between cargo versions, so the whole
/// tree is walked), keyed by the path below it.
fn outputs(target: &Path) -> BTreeMap<String, String> {
    let root = target.join("release");
    let mut m = BTreeMap::new();
    for p in walkdir_files(&root) {
        let name = p.to_string_lossy().into_owned();
        if [".rlib", ".rmeta", ".d"].iter().any(|x| name.ends_with(x)) {
            let rel = p
                .strip_prefix(&root)
                .expect("below root")
                .to_string_lossy()
                .replace('\\', "/");
            m.insert(
                rel,
                blake3::hash(&std::fs::read(&p).expect("read"))
                    .to_hex()
                    .to_string(),
            );
        }
    }
    assert!(
        !m.is_empty(),
        "the build left no outputs under {}",
        root.display()
    );
    m
}

fn run_app(target: &Path) -> String {
    let exe = target
        .join("release")
        .join(if cfg!(windows) { "app.exe" } else { "app" });
    let o = run(Command::new(exe), 30);
    assert!(o.ok, "the app failed to run");
    o.stdout
}

fn wipe(target: &Path) {
    // `cargo clean` of this target directory, without a cargo run that could
    // reach for the network.
    std::fs::remove_dir_all(target).expect("clean target");
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
    target: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let work = root.join("work");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixture"),
        &work,
    );
    Fixture {
        target: root.join("target"),
        _tmp: tmp,
        root,
        work,
    }
}

/// The crates of the fixture that are remote-eligible libraries: core_lib,
/// derive_mac (proc macro), embed and util.
const ELIGIBLE: u64 = 4;

fn app_text(work: &Path) -> String {
    format!(
        "answer=42 generated=from-build-script\nembedded-manifest-dir={}\nbuilt=yes\n",
        work.join("embed").to_string_lossy()
    )
}

/// A build, `cargo clean`, the same build again: every eligible crate comes
/// from the cache, the files are identical, the app runs and the warnings are
/// replayed. Jobs that ran on the peer were stored by BOTH nodes.
#[test]
fn a_clean_rebuild_is_served_from_the_cache() {
    let _g = serial();
    let f = fixture();
    let (n1, n2) = two_nodes(
        &f.root,
        "local_slots = 1\nprefer_remote = true",
        "local_slots = 2",
    );

    let b1 = build_ok(&n1, &f.work, &f.target);
    assert!(b1.stderr.contains("deliberately_unused"));
    let first = outputs(&f.target);
    assert_eq!(run_app(&f.target), app_text(&f.work));
    let c1 = cache(&n1.home);
    let remote1 = remote_ok(&n1.home);
    assert!(remote1 >= 3, "the first build should use the peer");
    assert_eq!((c1.local, c1.farm), (0, 0), "nothing to hit yet: {c1:?}");
    assert!(c1.misses >= ELIGIBLE, "{c1:?}");
    assert!(
        c1.stored >= ELIGIBLE,
        "every eligible crate is stored: {c1:?}"
    );
    // The executing node kept what it built (peers keep the entries of jobs
    // they executed).
    let c2 = cache(&n2.home);
    assert!(c2.stored >= 3, "the peer stored its jobs: {c2:?}");

    wipe(&f.target);
    let b2 = build_ok(&n1, &f.work, &f.target);
    let c1b = cache(&n1.home);
    assert!(
        c1b.local >= ELIGIBLE,
        "all eligible crates should be local hits: {c1b:?}\n{}",
        b2.stderr
    );
    assert_eq!(
        c1b.misses,
        c1.misses,
        "no new miss: {c1b:?}
{}",
        node_log(&n1.home)
    );
    assert_eq!(remote_ok(&n1.home), remote1, "no job went to the peer");
    // The same files, byte for byte, and the same program.
    assert_eq!(outputs(&f.target), first, "restored outputs differ");
    assert_eq!(run_app(&f.target), app_text(&f.work));
    // Warnings come back like from a real run: client paths, from the cache.
    assert!(b2.stderr.contains("deliberately_unused"), "{}", b2.stderr);
    assert!(
        b2.stderr.contains(&format!(
            "core_lib{}src{}lib.rs",
            std::path::MAIN_SEPARATOR,
            std::path::MAIN_SEPARATOR
        )),
        "{}",
        b2.stderr
    );
    // And cargo is satisfied: a third build recompiles nothing.
    let b3 = build_ok(&n1, &f.work, &f.target);
    assert!(!b3.stderr.contains("Compiling"), "{}", b3.stderr);
    let _ = n1.port;
}

/// Node A builds (everything runs on A); node B, another data directory but the
/// same client paths, builds the same workspace: its jobs are farm hits, served
/// by A, and B keeps them for next time.
#[test]
fn a_second_node_gets_the_first_nodes_results() {
    let _g = serial();
    let f = fixture();
    // B takes no jobs, so everything A builds runs on A and only A has entries.
    let (a, b) = two_nodes(&f.root, "local_slots = 4", "accept_remote_jobs = false");

    build_ok(&a, &f.work, &f.target);
    let first = outputs(&f.target);
    let ca = cache(&a.home);
    assert!(ca.stored >= ELIGIBLE, "{ca:?}");
    assert_eq!(cache(&b.home).stored, 0, "B has built nothing");

    wipe(&f.target);
    build_ok(&b, &f.work, &f.target);
    let cb = cache(&b.home);
    assert!(
        cb.farm >= ELIGIBLE,
        "B should get its libraries from A: {cb:?}\nB log:\n{}",
        node_log(&b.home)
    );
    assert_eq!(cb.local, 0, "{cb:?}");
    assert_eq!(remote_ok(&b.home), 0);
    assert!(cache(&a.home).served >= ELIGIBLE, "{:?}", cache(&a.home));
    assert_eq!(outputs(&f.target), first, "files from the farm differ");
    assert_eq!(run_app(&f.target), app_text(&f.work));

    // B kept what it fetched: the next clean build is local to B.
    wipe(&f.target);
    build_ok(&b, &f.work, &f.target);
    let cb2 = cache(&b.home);
    assert!(cb2.local >= ELIGIBLE, "{cb2:?}");
    assert_eq!(cb2.farm, cb.farm, "no second trip to the farm: {cb2:?}");
}

/// A `sensitive` path is never cached, in either direction.
#[test]
fn a_sensitive_path_is_never_cached() {
    let _g = serial();
    let f = fixture();
    let sensitive = f.work.to_string_lossy().replace('\\', "/");
    let n = single_node(&f.root, &format!("sensitive = [\"{sensitive}\"]"));

    build_ok(&n, &f.work, &f.target);
    wipe(&f.target);
    build_ok(&n, &f.work, &f.target);
    let c = cache(&n.home);
    assert_eq!(
        (c.local, c.farm, c.stored),
        (0, 0, 0),
        "a sensitive path entered the cache: {c:?}"
    );
    let mut cmd = crabfarm(&n.home);
    cmd.args(["cache", "stats"]);
    let stats = run(cmd, 20);
    assert!(stats.stdout.contains("0 entries"), "{}", stats.stdout);
}

/// Control for the test above: the same single node WITHOUT the sensitive
/// prefix does hit, so the sensitive test can only pass because of the prefix.
/// Also: a damaged cache file makes the build run normally, and the build
/// heals the cache.
#[test]
fn a_tampered_cache_file_means_a_normal_run_and_heals() {
    let _g = serial();
    let f = fixture();
    let n = single_node(&f.root, "");

    build_ok(&n, &f.work, &f.target);
    let first = outputs(&f.target);
    let stored = cache(&n.home).stored;
    assert!(stored >= ELIGIBLE);

    // Damage every file of the cache's store.
    let mut damaged = 0;
    for e in walkdir_files(&n.home.join("ccache").join("blobs")) {
        let mut raw = std::fs::read(&e).expect("read blob");
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        std::fs::write(&e, raw).expect("damage blob");
        damaged += 1;
    }
    assert!(damaged >= ELIGIBLE, "nothing to damage? {damaged}");

    wipe(&f.target);
    build_ok(&n, &f.work, &f.target);
    let c = cache(&n.home);
    assert_eq!(c.local, 0, "a damaged entry was used: {c:?}");
    assert!(c.unusable >= ELIGIBLE, "{c:?}");
    assert_eq!(
        outputs(&f.target),
        first,
        "a normal run gives the same files"
    );
    assert_eq!(run_app(&f.target), app_text(&f.work));

    // The rebuild stored fresh entries: the cache works again.
    wipe(&f.target);
    build_ok(&n, &f.work, &f.target);
    let c = cache(&n.home);
    assert!(c.local >= ELIGIBLE, "the cache did not heal: {c:?}");
}

fn walkdir_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walkdir_files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn cache_clear_empties_the_cache_and_stats_reads_it() {
    let _g = serial();
    let f = fixture();
    let n = single_node(&f.root, "");
    build_ok(&n, &f.work, &f.target);
    let mut c = crabfarm(&n.home);
    c.args(["cache", "stats"]);
    let before = run(c, 20);
    assert!(
        before.ok && !before.stdout.contains("0 entries"),
        "{}",
        before.stdout
    );
    assert!(
        before.stdout.contains("compile cache:"),
        "{}",
        before.stdout
    );
    let mut c = crabfarm(&n.home);
    c.args(["cache", "clear"]);
    assert!(run(c, 20).ok);
    let mut c = crabfarm(&n.home);
    c.args(["cache", "stats"]);
    assert!(run(c, 20).stdout.contains("0 entries"));
    // A clean build after `clear` is a miss everywhere.
    wipe(&f.target);
    build_ok(&n, &f.work, &f.target);
    assert_eq!(cache(&n.home).local, 0);
}
