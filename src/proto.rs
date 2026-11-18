//! Wire protocol: the single source of truth for messages between nodes and
//! between a wrapper and its local node (docs/DESIGN.md section 11).
//!
//! Frames are `u32` little-endian length + postcard-encoded [`Message`]
//! (see `transport::framing`). Max frame size is [`MAX_FRAME`].

use serde::{Deserialize, Serialize};

/// Bump on any incompatible change to these types.
pub const PROTO_VERSION: u16 = 2;
/// Maximum encoded frame size in bytes.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Blob transfer chunk size (plaintext bytes before compression).
pub const CHUNK_SIZE: usize = 1024 * 1024;
/// Default TCP port for peer connections.
pub const DEFAULT_PORT: u16 = 47070;

/// 32-byte BLAKE3 hash of a blob's plaintext.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BlobHash(pub [u8; 32]);

impl BlobHash {
    pub fn of(data: &[u8]) -> Self {
        BlobHash(*blake3::hash(data).as_bytes())
    }
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Debug for BlobHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BlobHash({})", &self.to_hex()[..16])
    }
}

/// Random 128-bit node id.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Debug)]
pub struct NodeId(pub [u8; 16]);

impl NodeId {
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Random 128-bit job id, chosen by the submitting node.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
pub struct JobId(pub [u8; 16]);

/// Path syntax of the submitting machine.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Debug)]
pub enum PathStyle {
    Windows,
    Posix,
}

/// An absolute, normalized path on the submitting machine (DESIGN section 6).
/// Windows: drive letter uppercased, no `\\?\` prefix, `\` separators.
/// Construct only through `paths::ClientPath::parse` (package A), which
/// normalizes; the fields are public for serialization.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Debug)]
pub struct ClientPath {
    pub style: PathStyle,
    pub path: String,
}

/// Identity of a rustc toolchain, from `rustc -vV` (DESIGN section 7).
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
pub struct ToolchainId {
    /// `release:` line, e.g. `1.100.0` or `1.101.0-nightly`.
    pub release: String,
    /// `commit-hash:` line.
    pub commit_hash: String,
    /// `commit-date:` line, e.g. `2026-11-10` (used to derive nightly specs).
    pub commit_date: String,
    /// `host:` line of the machine that produced this id.
    pub host: String,
}

impl ToolchainId {
    /// Same compiler build (host may differ).
    pub fn same_compiler(&self, other: &ToolchainId) -> bool {
        self.release == other.release && self.commit_hash == other.commit_hash
    }
}

/// One argument of a remote rustc command line. Paths are kept structured so
/// the executing peer can map them into its sandbox (DESIGN section 6).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum Arg {
    /// Passed verbatim.
    Plain(String),
    /// An absolute client path, mapped to the sandbox.
    Path(ClientPath),
    /// `prefix` immediately followed by a mapped absolute path, e.g.
    /// `-Ldependency=` + path, `--out-dir=` + path, `name=` + path for
    /// `--extern` values, `kind=` + path for `--emit` values.
    PrefixedPath { prefix: String, path: ClientPath },
    /// `--remap-path-prefix` value whose FROM part is a client path.
    RemapFrom { from: ClientPath, to: String },
}

/// One file the job needs in its sandbox.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct InputFile {
    pub path: ClientPath,
    pub hash: BlobHash,
    pub size: u64,
    pub executable: bool,
}

/// Everything a peer needs to run one rustc invocation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct JobSpec {
    pub job_id: JobId,
    pub toolchain: ToolchainId,
    /// Explicit target triple (the wrapper adds `--target` when cargo did not).
    pub target: String,
    /// Full argument list after argfile expansion, without the rustc path.
    /// Contains `--target`; does NOT contain the remap flags the peer adds.
    pub args: Vec<Arg>,
    pub cwd: ClientPath,
    /// Forwarded environment (DESIGN section 9). Values that are client
    /// paths are sent as `EnvValue::Path`.
    pub env: Vec<(String, EnvValue)>,
    /// Names in `env` beyond the fixed list of DESIGN section 9: the
    /// submitter's `forward_env` and the names it learned per crate. The
    /// executor accepts only the fixed list plus these (and never `PATH`,
    /// loader or wrapper variables, even when listed). Added with CF-8.
    pub extra_env: Vec<String>,
    pub inputs: Vec<InputFile>,
    /// Directories that must exist (out-dir, OUT_DIR, -L dirs).
    pub dirs: Vec<ClientPath>,
    /// The `--out-dir` (outputs are collected from here).
    pub out_dir: ClientPath,
    /// Crate name, for logs and status.
    pub crate_name: String,
    /// Delete this job's blobs after the job (sensitive prefix or retain off).
    pub no_retain: bool,
    /// Requires a peer with the same host triple (proc macros, until P3).
    pub needs_host: bool,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum EnvValue {
    Plain(String),
    Path(ClientPath),
}

/// Live state a node advertises to peers (DESIGN section 4).
#[derive(Clone, PartialEq, Serialize, Deserialize, Debug)]
pub struct Status {
    pub node_id: NodeId,
    pub node_name: String,
    pub host_triple: String,
    pub slots_total: u32,
    pub slots_busy: u32,
    pub queue_len: u32,
    /// Relative per-slot throughput, reference core = 100.
    pub speed: u32,
    pub toolchains: Vec<ToolchainId>,
    /// Installed targets per toolchain, parallel to `toolchains`.
    pub targets: Vec<Vec<String>>,
    pub accepting: bool,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct PeerInfo {
    pub node_id: NodeId,
    pub node_name: String,
    /// `host:port` strings the peer can be reached at.
    pub addrs: Vec<String>,
    /// Unix seconds.
    pub last_seen: u64,
}

/// Why a job ran locally instead of remotely (stats and logs).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
pub enum LocalReason {
    NotEligible,
    NoPeer,
    PeersBusy,
    UnknownDependency,
    TooLarge,
    LocalOnlyPath,
    RemoteFailed,
    RemoteCompileErrorRetry,
    EnvDependency,
    LocalSlotFree,
    /// The peer found its sandbox path inside an output (`env!("CARGO_MANIFEST_DIR")`
    /// and the like), which a local build would not contain. Appended in CF-10.
    SandboxPathInOutput,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    // ---- every connection ----
    Hello {
        proto: u16,
        node_id: NodeId,
        node_name: String,
        host_triple: String,
        crabfarm_version: String,
    },
    Error {
        message: String,
    },

    // ---- farm join (DESIGN section 3) ----
    JoinRequest {
        token: [u8; 16],
        node_name: String,
        /// Ed25519 public key (raw 32 bytes) the CA certifies. The node id is
        /// chosen by the CA, never by the joiner (protocol 2).
        public_key: Vec<u8>,
    },
    JoinAccepted {
        /// The node id the CA put into the certificate.
        node_id: NodeId,
        node_cert_der: Vec<u8>,
        ca_cert_der: Vec<u8>,
        known_peers: Vec<PeerInfo>,
    },

    // ---- control connection between peers ----
    PeerList {
        peers: Vec<PeerInfo>,
    },
    StatusUpdate(Status),
    /// Ask a peer to hold one slot for a job about to be sent.
    Reserve {
        job_id: JobId,
    },
    Reserved {
        job_id: JobId,
    },
    Busy {
        job_id: JobId,
    },

    // ---- job connection (submitter -> executor) ----
    JobRequest {
        spec: JobSpec,
        /// Submitter's blob key (encrypts this job's blobs at rest).
        blob_key: [u8; 32],
    },
    /// Executor -> submitter: blobs it does not have.
    NeedBlobs {
        hashes: Vec<BlobHash>,
    },
    /// Either direction. `data` is zstd-compressed plaintext of
    /// `[offset, offset + CHUNK_SIZE)`.
    BlobChunk {
        hash: BlobHash,
        offset: u64,
        last: bool,
        data: Vec<u8>,
    },
    /// Executor -> submitter: rustc started.
    Started,
    /// Executor -> submitter: one stderr line, already rewritten to client
    /// paths, without the trailing newline.
    Stderr {
        line: String,
    },
    /// Executor -> submitter: one stdout line (rare for rustc).
    Stdout {
        line: String,
    },
    /// Executor -> submitter: an output file follows as `BlobChunk`s (or is
    /// already known to the submitter when `hash` matches a local file).
    Output {
        path: ClientPath,
        hash: BlobHash,
        size: u64,
    },
    Exit {
        code: i32,
    },
    Cancel,

    // ---- IPC (wrapper <-> local node) ----
    IpcAuth {
        token: [u8; 32],
    },
    /// Wrapper -> node: a classified invocation. The node fills in inputs
    /// from `spec.args` and decides where it runs.
    Submit {
        spec: JobSpec,
        /// Path of the rustc cargo asked for (for local runs and -vV).
        rustc: String,
        /// The library files this invocation will produce and its
        /// `--extern` files, for the dependency graph (DESIGN section 8).
        produces: Vec<ClientPath>,
        externs: Vec<ClientPath>,
        /// Added in package D: what the node needs to collect the inputs
        /// (DESIGN section 8): the primary input file, `-L` dirs with their
        /// kind, and `-l` libraries (kind, name).
        input: ClientPath,
        search_dirs: Vec<(String, ClientPath)>,
        link_libs: Vec<(String, String)>,
    },
    /// Node -> wrapper: run rustc yourself; keep the IPC connection open
    /// until rustc exits (it holds a local slot), then send `LocalDone`.
    RunLocal {
        reason: LocalReason,
    },
    LocalDone {
        code: i32,
        produces: Vec<ClientPath>,
    },
    /// Wrapper -> node: request `Status` of all peers (crabfarm status).
    QueryStatus,
    FarmStatus {
        local: Status,
        peers: Vec<(PeerInfo, Option<Status>)>,
        counters: Vec<(String, u64)>,
    },
    /// Wrapper -> node (added in package D, appended so older variants keep
    /// their encoding): the remote result is not accepted (compile error of a
    /// non-registry crate, `# env-dep:` mismatch). The node answers
    /// `RunLocal { reason }` once a local slot is free.
    RetryLocal {
        reason: LocalReason,
    },
    /// Executor -> submitter (appended in CF-10): the output `path` contains the
    /// executor's sandbox path. The submitter discards every output of the job
    /// without writing it and reruns the job locally; the executor stops rustc.
    Tainted {
        path: ClientPath,
    },
}

/// Escape and truncate (200 characters) a string that came from a peer before
/// it goes into a log line or an error text: control characters such as
/// newlines or ANSI escapes must not forge log lines or garble a terminal.
pub fn log_safe(s: &str) -> String {
    const MAX: usize = 200;
    let mut out: String = s.chars().take(MAX).flat_map(char::escape_debug).collect();
    if s.chars().nth(MAX).is_some() {
        out.push_str("...");
    }
    out
}

struct Redacted;

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Byte buffers print as their length only.
struct Len(usize);

impl std::fmt::Debug for Len {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} bytes>", self.0)
    }
}

/// A job spec without its environment values and argument contents.
struct SpecSummary<'a>(&'a JobSpec);

impl std::fmt::Debug for SpecSummary<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.0;
        f.debug_struct("JobSpec")
            .field("job_id", &s.job_id)
            .field("crate_name", &s.crate_name)
            .field("toolchain", &s.toolchain)
            .field("target", &s.target)
            .field("args", &s.args.len())
            .field(
                "env",
                &s.env.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            )
            .field("inputs", &s.inputs.len())
            .finish_non_exhaustive()
    }
}

/// Hand-written so that tokens, keys and environment values can never reach a
/// log line through `{:?}`; every other variant stays readable. Peer-supplied
/// text is escaped and truncated.
impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Message::Hello {
                proto,
                node_id,
                node_name,
                host_triple,
                crabfarm_version,
            } => f
                .debug_struct("Hello")
                .field("proto", proto)
                .field("node_id", node_id)
                .field("node_name", &log_safe(node_name))
                .field("host_triple", &log_safe(host_triple))
                .field("crabfarm_version", &log_safe(crabfarm_version))
                .finish(),
            Message::Error { message } => f
                .debug_struct("Error")
                .field("message", &log_safe(message))
                .finish(),
            Message::JoinRequest {
                node_name,
                public_key,
                ..
            } => f
                .debug_struct("JoinRequest")
                .field("token", &Redacted)
                .field("node_name", &log_safe(node_name))
                .field("public_key", &Len(public_key.len()))
                .finish(),
            Message::JoinAccepted {
                node_id,
                node_cert_der,
                ca_cert_der,
                known_peers,
            } => f
                .debug_struct("JoinAccepted")
                .field("node_id", node_id)
                .field("node_cert_der", &Len(node_cert_der.len()))
                .field("ca_cert_der", &Len(ca_cert_der.len()))
                .field("known_peers", &known_peers.len())
                .finish(),
            Message::PeerList { peers } => f
                .debug_struct("PeerList")
                .field("peers", &peers.len())
                .finish(),
            Message::StatusUpdate(s) => f.debug_tuple("StatusUpdate").field(&s.node_id).finish(),
            Message::Reserve { job_id } => {
                f.debug_struct("Reserve").field("job_id", job_id).finish()
            }
            Message::Reserved { job_id } => {
                f.debug_struct("Reserved").field("job_id", job_id).finish()
            }
            Message::Busy { job_id } => f.debug_struct("Busy").field("job_id", job_id).finish(),
            Message::JobRequest { spec, .. } => f
                .debug_struct("JobRequest")
                .field("spec", &SpecSummary(spec))
                .field("blob_key", &Redacted)
                .finish(),
            Message::NeedBlobs { hashes } => f
                .debug_struct("NeedBlobs")
                .field("hashes", &hashes.len())
                .finish(),
            Message::BlobChunk {
                hash,
                offset,
                last,
                data,
            } => f
                .debug_struct("BlobChunk")
                .field("hash", hash)
                .field("offset", offset)
                .field("last", last)
                .field("data", &Len(data.len()))
                .finish(),
            Message::Started => f.write_str("Started"),
            Message::Stderr { line } => f
                .debug_struct("Stderr")
                .field("line", &log_safe(line))
                .finish(),
            Message::Stdout { line } => f
                .debug_struct("Stdout")
                .field("line", &log_safe(line))
                .finish(),
            Message::Output { path, hash, size } => f
                .debug_struct("Output")
                .field("path", path)
                .field("hash", hash)
                .field("size", size)
                .finish(),
            Message::Exit { code } => f.debug_struct("Exit").field("code", code).finish(),
            Message::Cancel => f.write_str("Cancel"),
            Message::Tainted { path } => f.debug_struct("Tainted").field("path", path).finish(),
            Message::IpcAuth { .. } => f.debug_struct("IpcAuth").field("token", &Redacted).finish(),
            Message::Submit {
                spec,
                rustc,
                produces,
                externs,
                input,
                search_dirs,
                link_libs,
            } => f
                .debug_struct("Submit")
                .field("spec", &SpecSummary(spec))
                .field("rustc", rustc)
                .field("produces", &produces.len())
                .field("externs", &externs.len())
                .field("input", input)
                .field("search_dirs", &search_dirs.len())
                .field("link_libs", &link_libs.len())
                .finish(),
            Message::RunLocal { reason } => {
                f.debug_struct("RunLocal").field("reason", reason).finish()
            }
            Message::LocalDone { code, produces } => f
                .debug_struct("LocalDone")
                .field("code", code)
                .field("produces", &produces.len())
                .finish(),
            Message::QueryStatus => f.write_str("QueryStatus"),
            Message::FarmStatus {
                local,
                peers,
                counters,
            } => f
                .debug_struct("FarmStatus")
                .field("local", &local.node_id)
                .field("peers", &peers.len())
                .field("counters", &counters.len())
                .finish(),
            Message::RetryLocal { reason } => f
                .debug_struct("RetryLocal")
                .field("reason", reason)
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_secrets_and_keeps_the_rest_readable() {
        let join = format!(
            "{:?}",
            Message::JoinRequest {
                token: [0xAB; 16],
                node_name: "beta".into(),
                public_key: vec![9; 32]
            }
        );
        assert!(!join.contains("171") && !join.contains("ab"), "{join}");
        assert!(join.contains("beta") && join.contains("redacted"));
        let ipc = format!("{:?}", Message::IpcAuth { token: [200; 32] });
        assert!(!ipc.contains("200"), "{ipc}");
        let mut key = [0u8; 32];
        key[0] = 77;
        key[31] = 88;
        let spec = JobSpec {
            job_id: JobId([1; 16]),
            toolchain: ToolchainId {
                release: "1.100.0".into(),
                commit_hash: "h".into(),
                commit_date: "d".into(),
                host: "x".into(),
            },
            target: "t".into(),
            args: vec![],
            cwd: ClientPath {
                style: PathStyle::Posix,
                path: "/".into(),
            },
            env: vec![("SECRET_VAR".into(), EnvValue::Plain("hunter2".into()))],
            inputs: vec![],
            dirs: vec![],
            out_dir: ClientPath {
                style: PathStyle::Posix,
                path: "/o".into(),
            },
            crate_name: "mycrate".into(),
            no_retain: false,
            needs_host: false,
            extra_env: vec![],
        };
        let job = format!(
            "{:?}",
            Message::JobRequest {
                spec,
                blob_key: key
            }
        );
        assert!(
            !job.contains("hunter2") && !job.contains("77") && !job.contains("88"),
            "{job}"
        );
        assert!(job.contains("mycrate") && job.contains("SECRET_VAR"));
        // Other variants stay readable.
        assert_eq!(
            format!("{:?}", Message::Exit { code: 3 }),
            "Exit { code: 3 }"
        );
        assert_eq!(format!("{:?}", Message::Started), "Started");
    }

    #[test]
    fn peer_text_is_escaped_and_truncated() {
        assert_eq!(log_safe("a\nb\x1b[31m"), "a\\nb\\u{1b}[31m");
        let long = "x".repeat(500);
        let out = log_safe(&long);
        assert_eq!(out.len(), 203);
        assert!(out.ends_with("..."));
        let e = format!(
            "{:?}",
            Message::Error {
                message: format!("{long}\nFORGED")
            }
        );
        assert!(e.len() < 260 && !e.contains("FORGED"), "{e}");
    }
}
