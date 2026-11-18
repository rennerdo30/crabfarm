//! CF-19: one node per data directory. The second `crabfarm node` must exit
//! with a clear message and leave everything of the first one alone: its IPC
//! port and token, its job sandboxes (the sweep at start would delete them).

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_crabfarm");

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn crabfarm(home: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.env("CRABFARM_HOME", home).env_remove("CRABFARM_LOG");
    c
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

#[test]
fn a_second_node_on_the_same_directory_exits_and_touches_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "node_name = \"one\"\nlisten = \"127.0.0.1:{}\"\nmdns = false\nslots = 2\nlocal_slots = 1\nauto_install = false\n",
            free_port()
        ),
    )
    .unwrap();
    let status = crabfarm(&home)
        .args(["farm", "create", "--name", "one"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "farm create failed");

    let first = Proc(
        crabfarm(&home)
            .arg("node")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    while !home.join("ipc.port").exists() {
        assert!(Instant::now() < deadline, "the first node did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    // A sandbox the start-up sweep of a second node would remove.
    let marker = home.join("jobs").join("running-job");
    std::fs::create_dir_all(&marker).unwrap();
    std::fs::write(marker.join("plain.rs"), b"plaintext source").unwrap();
    let port_before = std::fs::read(home.join("ipc.port")).unwrap();
    let token_before = std::fs::read(home.join("ipc.token")).unwrap();

    let mut second = crabfarm(&home)
        .arg("node")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(s) = second.try_wait().unwrap() {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = second.kill();
            let _ = second.wait();
            panic!("the second node did not exit: it is running next to the first");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = {
        let mut s = String::new();
        if let Some(mut e) = second.stderr.take() {
            std::io::Read::read_to_string(&mut e, &mut s).ok();
        }
        s
    };
    assert!(!status.success(), "the second node must fail");
    assert!(
        out.contains("another crabfarm node is already running"),
        "unclear message: {out}"
    );
    assert!(
        marker.join("plain.rs").exists(),
        "the second node swept the first one's job sandbox"
    );
    assert_eq!(std::fs::read(home.join("ipc.port")).unwrap(), port_before);
    assert_eq!(std::fs::read(home.join("ipc.token")).unwrap(), token_before);
    // The first node still serves.
    let alive = Command::new(BIN)
        .env("CRABFARM_HOME", &home)
        .arg("status")
        .output()
        .unwrap();
    assert!(alive.status.success(), "the first node stopped answering");
    drop(first);
}
