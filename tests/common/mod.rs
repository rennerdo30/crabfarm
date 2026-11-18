//! Shared helpers of the wrapper-level integration tests: a fake node that
//! speaks the IPC protocol, and a runner for the real `crabfarm` binary in
//! wrapper mode. Loopback only; every child has a timeout and is killed by its
//! own handle.

#![allow(dead_code)]

use crabfarm::proto::{JobSpec, LocalReason, Message};
use crabfarm::transport::FramedConn;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};

pub const BIN: &str = env!("CARGO_BIN_EXE_crabfarm");

/// What the fake node does with the n-th `Submit` it receives (the last entry
/// repeats).
#[derive(Clone)]
pub enum Behavior {
    /// Answer `RunLocal` and wait until the wrapper reports `LocalDone`.
    RunLocal,
    /// Pretend a peer built the job: put `dep_info` in the out-dir under
    /// `dep_file_name` (what the submitter does with a peer's `.d`), answer
    /// `Exit { code: 0 }`, and serve a `RetryLocal` with `RunLocal`.
    PeerSuccess {
        dep_file_name: String,
        dep_info: String,
    },
    /// Pretend a peer failed to compile: send `stderr_line` as a diagnostic and
    /// `Exit { code: 1 }`, and serve a `RetryLocal` with `RunLocal`.
    PeerCompileError { stderr_line: String },
    /// Accept the `Submit` and never say anything again.
    Silent,
}

pub struct Submitted {
    pub spec: JobSpec,
    pub force_local: bool,
}

pub struct FakeNode {
    pub submits: Arc<Mutex<Vec<Submitted>>>,
    /// How many `RetryLocal` frames the wrapper sent.
    pub retries: Arc<std::sync::atomic::AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeNode {
    /// Publish `ipc.port` and `ipc.token` in `home`, like a running node.
    pub async fn start(home: &Path, behaviors: Vec<Behavior>) -> FakeNode {
        std::fs::create_dir_all(home).expect("home");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let mut token = [0u8; 32];
        for (i, b) in token.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(3);
        }
        std::fs::write(home.join("ipc.token"), token).expect("token");
        std::fs::write(home.join("ipc.port"), port.to_string()).expect("port");
        let submits: Arc<Mutex<Vec<Submitted>>> = Arc::default();
        let log = submits.clone();
        let retries: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
        let retry_count = retries.clone();
        let task = tokio::spawn(async move {
            let mut held: Vec<FramedConn<TcpStream>> = Vec::new();
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let mut conn = FramedConn::new(tcp);
                match conn.recv().await {
                    Ok(Some(Message::IpcAuth { token: got })) if got == token => {}
                    _ => continue,
                }
                let Ok(Some(Message::Submit {
                    spec, force_local, ..
                })) = conn.recv().await
                else {
                    continue;
                };
                let n = {
                    let mut l = log.lock().unwrap();
                    l.push(Submitted {
                        spec: spec.clone(),
                        force_local,
                    });
                    l.len() - 1
                };
                let behavior = behaviors[n.min(behaviors.len() - 1)].clone();
                match behavior {
                    Behavior::RunLocal => {
                        let _ = conn
                            .send(&Message::RunLocal {
                                reason: LocalReason::NoPeer,
                            })
                            .await;
                        drain(&mut conn).await;
                    }
                    Behavior::PeerSuccess {
                        dep_file_name,
                        dep_info,
                    } => {
                        let dep = spec.out_dir.to_local().join(dep_file_name);
                        std::fs::write(&dep, dep_info).expect("write the peer's .d");
                        let _ = conn.send(&Message::Exit { code: 0 }).await;
                        if let Ok(Some(Message::RetryLocal { .. })) = conn.recv().await {
                            let _ = conn
                                .send(&Message::RunLocal {
                                    reason: LocalReason::EnvDependency,
                                })
                                .await;
                            drain(&mut conn).await;
                        }
                    }
                    Behavior::PeerCompileError { stderr_line } => {
                        let _ = conn.send(&Message::Stderr { line: stderr_line }).await;
                        let _ = conn.send(&Message::Exit { code: 1 }).await;
                        if let Ok(Some(Message::RetryLocal { .. })) = conn.recv().await {
                            retry_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let _ = conn
                                .send(&Message::RunLocal {
                                    reason: LocalReason::RemoteCompileErrorRetry,
                                })
                                .await;
                            drain(&mut conn).await;
                        }
                    }
                    Behavior::Silent => held.push(conn),
                }
            }
        });
        FakeNode {
            submits,
            retries,
            task,
        }
    }

    pub fn submitted(&self) -> usize {
        self.submits.lock().unwrap().len()
    }

    /// Environment names of the n-th submitted job.
    pub fn env_names(&self, n: usize) -> Vec<String> {
        self.submits.lock().unwrap()[n]
            .spec
            .env
            .iter()
            .map(|(k, _)| k.clone())
            .collect()
    }
}

/// Read until the wrapper is done with its local run.
async fn drain(conn: &mut FramedConn<TcpStream>) {
    loop {
        match conn.recv().await {
            Ok(Some(Message::LocalDone { .. })) | Ok(None) | Err(_) => return,
            Ok(Some(_)) => {}
        }
    }
}

pub struct Ran {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

/// A throwaway crate directory: `src/lib.rs` with `lib_source` and an `out`
/// directory. Returns its path.
pub fn make_crate(root: &Path, lib_source: &str) -> PathBuf {
    let work = root.join("work");
    std::fs::create_dir_all(work.join("src")).expect("src");
    std::fs::create_dir_all(work.join("out")).expect("out");
    std::fs::write(work.join("src").join("lib.rs"), lib_source).expect("lib.rs");
    work
}

/// The wrapper's argument list for `make_crate` (what cargo would pass).
pub fn wrapper_args() -> Vec<&'static str> {
    vec![
        "rustc",
        "--crate-name",
        "x",
        "--edition=2021",
        "src/lib.rs",
        "--crate-type",
        "lib",
        "--emit=dep-info,metadata,link",
        "--out-dir",
        "out",
        "-C",
        "extra-filename=-abc",
    ]
}

/// Run `crabfarm rustc ...` in `work` with `CRABFARM_HOME=home` plus `env`,
/// within `secs`, or kill exactly that child and fail the test.
pub fn run_wrapper(home: &Path, work: &Path, env: &[(&str, &str)], secs: u64) -> Ran {
    let mut c = Command::new(BIN);
    c.args(wrapper_args())
        .current_dir(work)
        .env("CRABFARM_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Nothing from the surrounding `cargo test` may leak into the run, except
    // where rustup and cargo find their own files.
    for (k, _) in std::env::vars() {
        let up = k.to_ascii_uppercase();
        if (up.starts_with("CARGO_") && up != "CARGO_HOME")
            || up.starts_with("RUSTC")
            || up == "RUSTFLAGS"
            || up == "CRABFARM_LOG"
            || up.ends_with("_TOKEN")
        {
            c.env_remove(k);
        }
    }
    for (k, v) in env {
        c.env(k, v);
    }
    let started = Instant::now();
    let mut child = c.spawn().expect("spawn the wrapper");
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
    let deadline = started + Duration::from_secs(secs);
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let stdout = String::from_utf8_lossy(&t_out.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&t_err.join().unwrap_or_default()).into_owned();
    assert!(
        status.is_some(),
        "the wrapper did not finish within {secs}s\nstderr:\n{stderr}"
    );
    Ran {
        code: status.and_then(|s| s.code()),
        stdout,
        stderr,
        elapsed: started.elapsed(),
    }
}
