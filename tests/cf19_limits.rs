//! CF-19 limits of the real node, over loopback.

use std::io::Read;
use std::net::TcpStream;
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

fn start_node(tmp: &Path) -> (std::path::PathBuf, Proc) {
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    std::fs::write(
        home.join("config.toml"),
        format!(
            "node_name = \"lim\"\nlisten = \"127.0.0.1:{port}\"\nmdns = false\nslots = 2\nlocal_slots = 1\nauto_install = false\n"
        ),
    )
    .unwrap();
    let st = crabfarm(&home)
        .args(["farm", "create", "--name", "lim"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(st.success());
    let node = Proc(
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
        assert!(Instant::now() < deadline, "the node did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    (home, node)
}

/// A connection that sends nothing is closed by the node after 2 s; but while
/// 16 of them wait, further ones are dropped at accept.
#[test]
fn ipc_connections_waiting_for_auth_are_capped() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _node) = start_node(tmp.path());
    let port: u16 = std::fs::read_to_string(home.join("ipc.port"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    let total = crabfarm::node::MAX_IPC_UNAUTHENTICATED + 16;
    let mut socks: Vec<TcpStream> = (0..total)
        .map(|_| TcpStream::connect(("127.0.0.1", port)).unwrap())
        .collect();
    std::thread::sleep(Duration::from_millis(500));
    // A closed connection reads EOF (or a reset) at once; one the node still
    // holds would block.
    let (mut open, mut closed) = (0, 0);
    for s in &mut socks {
        if is_closed(s) {
            closed += 1;
        } else {
            open += 1;
        }
    }
    assert!(
        open <= crabfarm::node::MAX_IPC_UNAUTHENTICATED,
        "{open} connections wait for auth at once ({closed} closed)"
    );
    assert!(open >= 1, "the first connections must be served");

    // After the wait times out, an authenticated wrapper gets through.
    drop(socks);
    std::thread::sleep(Duration::from_millis(2500));
    let status = Command::new(BIN)
        .env("CRABFARM_HOME", &home)
        .arg("status")
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "the node does not serve after the flood"
    );
}

/// True when the node has closed `s` (a read returned EOF or reset, not a timeout).
fn is_closed(s: &mut TcpStream) -> bool {
    let mut b = [0u8; 1];
    s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
    match s.read(&mut b) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => !matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
    }
}
