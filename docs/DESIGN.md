# crabfarm design

crabfarm spreads `rustc` work from one machine over a peer-to-peer **farm** of machines (Windows, macOS, Linux, mixed). It is a single binary. Every machine runs the same **node**; any node both submits jobs and runs jobs for others.

Status of this document: v1 design, 2026-11-19. Phases are marked **P1** (first working version: Windows to Windows), **P2** (cross-OS jobs without proc macros, result cache, live status), **P3** (cross-OS proc macros). Everything marked P1 is the acceptance target of the first build round.

## 1. Goals and non-goals

Goals
- `crabfarm cargo build` on a weak machine uses the CPU of the other machines in the farm, with output identical to a local build (same `.rlib`/`.rmeta` semantics, same diagnostics with local paths, pipelined builds keep working).
- Zero cargo configuration: no change to the user's projects; works with any cargo command (`build`, `check`, `test`, `clippy`, `run`, `doc` is local).
- Safe by default: every byte between machines is encrypted and mutually authenticated; only machines that joined the farm can submit or run jobs.
- Fails safe: any infrastructure problem (peer gone, toolchain missing, network error, unsupported flag) silently runs the job locally. A remote result is only used when it is known to be equivalent.
- Cross-platform from day one: protocol and node code have no OS assumptions; path handling is explicit.

Non-goals (v1)
- No NAT traversal or relay: peers reach each other on the LAN or over a VPN (Tailscale/WireGuard) by IP or name.
- No distributed linking: final binaries, tests, cdylibs and dylibs link on the submitting machine.
- No Unity, C/C++ (`cc` crate) or build-script distribution: cargo runs build scripts locally as usual.
- No sandbox against malicious farm members: all farm members trust each other fully (proc macros and build-time code run on the executing peer).

## 2. Roles and processes

One binary, `crabfarm`:

| Command | Purpose |
|---|---|
| `crabfarm node` | The daemon. Serves jobs to peers (TCP, mTLS) and to local wrappers (loopback IPC). Foreground; `--detach` in P2. |
| `crabfarm farm create [--name N]` | Create a new farm on this machine (farm CA). |
| `crabfarm farm invite` | Print a one-time join code (valid 10 minutes, single use). |
| `crabfarm farm join <code>` | Join a farm. |
| `crabfarm cargo <args...>` | Run cargo with `RUSTC_WRAPPER=crabfarm` and a farm-sized `-j`. |
| `crabfarm status` | Peers, slots, running jobs, local/remote counters. |
| `crabfarm <path-to-rustc> <rustc args...>` | Wrapper mode (cargo calls this). Detected when argv[1] is a path whose file stem is `rustc` (or `clippy-driver`). |

Process flow for one rustc invocation:

```
cargo ──spawn──> crabfarm (wrapper) ──IPC──> local node ──mTLS──> peer node ──spawn──> rustc
   ^                 │  (runs rustc itself on RunLocal)              │
   └── stdout/stderr ┘ <────────── stderr lines, artifacts, outputs ─┘
```

- The wrapper is short-lived and stateless. It classifies the invocation (section 5). If it is not remote-eligible, or the local node is not running, it executes rustc directly with inherited stdio and exits with its code.
- Otherwise it sends a `Submit` to the local node over IPC. The node either answers `RunLocal { permit }` (the wrapper runs rustc itself, holding a local slot until it disconnects) or runs the job on a peer and streams the result back.

## 3. Farm identity and security

- **Farm CA**: `farm create` generates an Ed25519 CA key and self-signed CA certificate (rcgen). CA key file stays on the creating node (`ca.key`, file permissions owner-only). The farm id is the SHA-256 of the CA certificate DER; its first 16 bytes in lowercase hex are the *short farm id*.
- **Node identity**: each node has an Ed25519 key and a certificate signed by the CA. Subject CN = node name (default: hostname); SAN contains the node id (random 128-bit, hex). Validity 10 years in P1. Revocation (P2): CA-signed revocation list gossiped between peers.
- **Join code** format: `cf1-<token>-<fp>@<host>:<port>`, where `<token>` is 16 random bytes and `<fp>` is the first 16 bytes of the inviter node's **certificate** SHA-256, both in unpadded lowercase base32 (RFC 4648 alphabet, `data-encoding` `BASE32_NOPAD` lowercased). The joiner connects with TLS, pins the inviter's cert by `<fp>` (it has no CA yet), sends `JoinRequest { token, node_name, csr_public_key }`; the inviter checks the token (single use, 10-minute expiry, constant-time compare), signs a node certificate, and returns `JoinAccepted { node_cert, ca_cert, known_peers }`. Only a node holding the CA key can issue invites in P1. Invites live in `farm/invites.toml` as SHA-256 hashes of the tokens (so the CLI process and the node process share them without the file holding a redeemable token). Every connection's `Hello` must carry the node id of the certificate it arrived under; only a `JoinRequest` first frame is accepted without a client certificate.
- **Transport**: TCP + rustls, **TLS 1.3 only**, ring provider, mutual authentication. Both sides require a peer certificate chaining to the farm CA (custom verifier: single-level chain, CA pinned by DER equality, validity checked, ignores hostnames). There is no plaintext or anonymous mode.
- **Local IPC**: TCP on `127.0.0.1:<ipc_port>` with a 32-byte random token from `ipc.token` in the node data directory (owner-only permissions, regenerated at each node start). The first frame on an IPC connection must carry the token; otherwise the node closes the connection.
- **At rest on the executing peer**: blobs a peer keeps for later jobs are encrypted with the *submitting* node's blob key (`blob.key`, 32 random bytes, never written on other peers; sent inside the TLS session with each job). XChaCha20-Poly1305, random 24-byte nonce per blob, the blob hash as associated data. Plaintext exists only in the job sandbox while rustc runs; the sandbox is deleted when the job ends (success or failure). Nodes may set `retain_blobs = false` and per-path `sensitive` prefixes (config) so blobs from those submissions are deleted after the job. Recommendation for the owner: BitLocker/FileVault/LUKS on every farm machine, because rustc must read plaintext sources during the job.
- **Trust model**: any farm member can run arbitrary code on any other member via proc macros. Only join machines you own.

Data directory (via `directories`, `ProjectDirs::from("dev", "renner", "crabfarm")`):
`config.toml`, `farm/ca.crt`, `farm/ca.key` (creator only), `node.key`, `node.crt`, `node.id`, `blob.key`, `ipc.token`, `ipc.port`, `peers.toml` (learned peers), `cache/` (blob store), `jobs/` (sandboxes).

## 4. Discovery, membership and load balancing

- **mDNS** (`mdns-sd`): service type `_crabfarm._tcp.local.`, instance name = node id, TXT: `f=<short farm id>`, `v=<protocol version>`. Nodes ignore other farms.
- **Static peers**: `config.toml` `peers = ["desktop.tailnet:47070", "10.0.0.5:47070"]` for VPN/remote use.
- **Gossip**: on each connection, both sides exchange `PeerList` (node id, name, addresses, last seen). Learning one address is enough to find the whole farm. Learned peers persist in `peers.toml`.
- **Status**: each node keeps a TLS control connection to every reachable peer and receives `Status` pushes every second while either side has active work, every 10 s otherwise:
  `Status { node_id, host_triple, slots_total, slots_busy, queue_len, speed, toolchains: [ToolchainId], targets: [triple], accepting }`.
  `speed` is a relative per-slot throughput score measured once at node start (P1: a fixed 300 ms single-thread benchmark, normalized so a reference core is 100).
- **Scheduling** (on the submitting node, per job):
  1. Candidates are peers that are reachable, `accepting`, and can run the job now: toolchain installed (or installable, see section 7: an installing peer is not a candidate for that job), target installed, and (proc-macro jobs or jobs with `--extern` proc macros) `host_triple == submitter host_triple` until P3.
  2. Local slots count as a candidate (the local node itself) with link cost 0.
  3. Score = `free_slots * speed - link_penalty(upload_bytes)`, where `link_penalty` uses the measured peer bandwidth (EWMA of recent transfers; default 100 Mbit/s until measured). Pick the max; ties go local.
  4. A slot is reserved on the chosen peer with `Reserve`; if the peer refuses (full), retry the next candidate once, then run locally.
- **Backpressure**: a node never queues more remote jobs than `slots_total`; it refuses with `Busy`. Its own local wrappers always get a `RunLocal` permit when local slots are free, else they wait in a FIFO.
- **`crabfarm cargo -j`**: if the user passes no `-j`/`--jobs`, `crabfarm cargo` sets `-j` = local slots + sum of free remote slots of reachable peers, capped at 128. Cargo's jobserver then admits enough wrappers; the node's local permit queue keeps local rustc processes within local slots.

## 5. Classifying a rustc invocation (wrapper)

Expand `@argfile` arguments first (cargo uses them on Windows for long command lines; one argument per line, UTF-8).

Always local (passthrough, no node contact):
- `-vV`, `--version`, any `--print`, input `-` (stdin), no input file.
- `--crate-type` contains `bin`, `dylib`, `cdylib`, `staticlib` (anything that links), or any test harness build (`--test`).
- `--emit` contains anything besides `dep-info`, `metadata`, `link`.
- `-C incremental=...` unless `config.toml` `remote_incremental = "strip"` (then the flag is removed for the remote run; P1 default `"local"`).
- `-C target-cpu=native`, `-C profile-generate`, `-C profile-use`, `-Z` flags not on the allow list (P1 allow list: empty), `--sysroot`, unknown flags that take a path.
- Crate-type `proc-macro` or `lib` with `--extern` of a proc-macro library when no same-host peer is available (scheduler decides; wrapper marks the job `needs_host = true`).
- Path in a `config.toml` `local_only` prefix list.

Remote-eligible: `--crate-type lib|rlib|proc-macro` (default `lib` when absent with `--crate-name` and a `.rs` input), emit subset of `{dep-info, metadata, link}`.

Arguments that carry paths and must be mapped (section 6): the input file, `--out-dir`, `-o`, `-L [kind=]path`, `--extern name=path` (and `--extern name` without path stays as is), `--emit kind=path`, `-C linker=` (absolute path is dropped on a remote of a different host or a missing file; bare name kept), existing `--remap-path-prefix FROM=TO` (FROM mapped), `-C link-arg`/`-C link-args` (kept verbatim; only matters for proc-macro linking on same-host peers).

Target: if no `--target` is given, the wrapper adds `--target <client host triple>` for the remote run (a remote of another OS would otherwise build for its own host). Cargo's sysroot detection is not affected because the explicit target equals the default.

## 6. Paths

- `ClientPath`: an absolute, normalized path string as seen on the submitting machine plus its style (`Windows` or `Posix`). Windows paths: drive letter uppercased, `\\?\` prefix removed, separators `\`.
- Sandbox mapping on the executing peer, job root `R = <data>/jobs/<job id>/root`:
  - Windows `C:\a\b` → `R/C/a/b`; UNC `\\server\share\a` → `R/UNC/server/share/a`.
  - Posix `/a/b` → `R/_/a/b`.
  - Mapping is a pure function in both directions; tests cover round trips.
- Relative arguments stay relative; rustc runs with `cwd` = mapped client cwd.
- `--remap-path-prefix`: for each drive/root that appears in the job, add `--remap-path-prefix <mapped root>=<client root>` **before** the user's own remap flags (rustc applies the last matching flag, so the user's flags still win).
- **stderr rewriting**: every stderr line (JSON diagnostics and rendered text) has mapped prefixes replaced by client prefixes; for a Windows client, `/` after a replaced prefix in the same path token is converted to `\` (best effort; JSON strings are rewritten after parsing, not by raw text replace).
- **dep-info (`.d`) rewriting**: output `.d` files are rewritten to client paths before they are written locally (cargo fingerprints from them). Escaped spaces (`\ `) are preserved.
- Environment values that start with a mapped job path are mapped (`OUT_DIR`, `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH`, `CARGO_TARGET_TMPDIR`, `CARGO_BIN_EXE_*`, `CARGO_RUSTC_CURRENT_DIR`).

## 7. Toolchains

- `ToolchainId { release: String, commit_hash: String, host: String }` from `rustc -vV` (`release`, `commit-hash`, `host`). The executing peer must have a toolchain with the same `release` and `commit_hash`; its own `host` may differ (P2/P3).
- The submitting node caches `rustc -vV` per rustc path + file size + mtime.
- Executing peer resolves the toolchain via `rustup toolchain list` and `rustc +<name> -vV`, cached. Missing: if `auto_install = true` (default), run `rustup toolchain install <spec> --profile minimal --no-self-update` in the background, where `<spec>` = `release` for stable/beta, `nightly-<commit-date + 1 day>` for nightly (verified by commit hash after install). Missing target: `rustup target add --toolchain <name> <target>`. While installing, the peer reports the toolchain as not installed, so jobs run elsewhere.
- Rustup child processes have a 30-minute timeout and are recorded by PID; only those are ever killed.

## 8. Blob store and inputs

- Content addressing: BLAKE3 of the plaintext, 32 bytes. Transfer: zstd level 3, 1 MiB chunks.
- **Inputs of a job**:
  1. Source files: all regular files under the package directory (`CARGO_MANIFEST_DIR`, or the input file's directory when not set), skipping `target/`, `.git/`, any directory starting with `.`, and files over 64 MiB. Hard cap: 256 MiB total, else local.
  2. `OUT_DIR` contents (recursively) if set.
  3. Dependency libraries: every file passed with `--extern`, plus the **transitive closure** that rustc finds through `-L dependency=` (it needs the metadata of indirect dependencies, and proc macros re-exported by a dependency, e.g. `serde` re-exporting `serde_derive`). The closure comes from the node's **dependency graph**: for every rustc invocation that goes through the wrapper (local or remote), the node records `produced library file → its --extern files` (keyed by client path, with size and mtime). Closure = transitive walk of that map from the job's `--extern` files. If any file in the walk is unknown (built before crabfarm was used) or changed since it was recorded, the job runs locally (and is recorded). If the closure contains a proc-macro library, the job gets `needs_host = true`.
  4. Native libraries: for each `-l [static=]name`, the file `name.lib` (msvc) or `libname.a` (others) found in `-L native=`/`-L all=`/`-L` directories.
- The submitting node caches file hashes by (path, size, mtime) so unchanged files cost no hashing.
- **Have/need**: the submitter sends the manifest (path, hash, size, executable); the peer answers `NeedBlobs [hash]` for blobs not in its store; the submitter streams those. Outputs a peer produced stay in its store (encrypted with the submitter's key), so dependency rlibs are almost never uploaded.
- **Store**: `cache/<first 2 hex>/<hash hex>`, encrypted (section 3). LRU eviction above `cache_max_gb` (default 20).

## 9. Running a job on a peer

1. Resolve toolchain; ensure target.
2. Materialize the sandbox: decrypt each input blob into its mapped path (hard links are not used; files are written). Create mapped `--out-dir` and `OUT_DIR`.
3. Build the argument list (mapped paths, added `--target`, added remaps, `-C incremental` stripped if configured), environment (forwarded variables from the job, mapped paths), `cwd`.
4. Spawn `rustc` of the resolved toolchain (direct path to the toolchain's `bin/rustc`, not the rustup proxy) with a node-wide **jobserver** (`jobserver::Client::new(slots_total)`, passed with `configure`) so codegen threads of all jobs share the peer's cores. Timeout: `job_timeout_min` (default 30).
5. Stream stderr line by line to the submitter, rewritten (section 6). When a line is an artifact notification (`{"artifact":"<path>","emit":"metadata"}` etc.), send that file as an `Output` first, then the line, so the submitter writes the file before cargo sees the notification (pipelining).
6. On exit: send every new or changed file in the mapped out-dir (rewriting `.d` files), then `Exit { code }`. Store outputs in the blob store under the submitter's key. Delete the sandbox.

Environment forwarded from the wrapper: all `CARGO_*`, `OUT_DIR`, `RUSTC_BOOTSTRAP`, `RUSTFLAGS`-derived nothing (cargo already expanded them into args), `DEP_*`, `TARGET`, `HOST`, `PROFILE`, `NUM_JOBS`, `OPT_LEVEL`, `DEBUG`, plus names listed in the config `forward_env`. After a remote success, the submitter parses the `# env-dep:` lines of the returned `.d`: if a listed variable is set locally but was not forwarded, the job is rerun locally and the name is remembered per crate (node state) so later runs forward it.

## 10. Results and fallback

- Remote exit code 0: outputs are written to the client paths (atomic rename per file), stderr was already forwarded, wrapper exits 0.
- Remote nonzero exit with diagnostics: the error is authoritative for registry crates (path under `CARGO_HOME`) and forwarded as is. For other crates (workspace, git, path deps), the wrapper reruns locally with a clean stderr (the remote stderr is buffered, not forwarded, for these crates; this costs at most one duplicate compile on a real error, and makes missing-input problems invisible to the user).
- Infrastructure failure at any point before the first stderr line was forwarded: rerun locally. After stderr began (registry crates): the wrapper buffers nothing more; it reports `crabfarm: remote job failed (<reason>), rerunning locally` once to stderr and reruns.
- Every fallback is counted per reason in node stats.

## 11. Wire protocol

- Frame: `u32` little-endian length + postcard-encoded `Message`. Max frame 16 MiB. Blob chunks are `BlobChunk { hash, offset, last, data }` frames.
- Every connection starts with `Hello { proto: u16, node_id, node_name, host_triple, crabfarm_version }` from both sides; a different `proto` closes the connection with `Error`.
- Job connection (P1: one TLS connection per job, rustls session resumption on): `JobRequest` → (`NeedBlobs` ↔ `BlobChunk`*) → `Started` → (`Stderr` | `Output` + `BlobChunk`*)* → `Exit`. Either side may send `Cancel`/`Error`.
- Message types are defined in `src/proto.rs`; it is the single source of truth.

## 12. Configuration (`config.toml`)

```toml
node_name = "desktop"          # default: hostname
listen = "0.0.0.0:47070"
slots = 32                      # default: logical CPUs
accept_remote_jobs = true
peers = []                      # static peer addresses
auto_install = true
remote_incremental = "local"    # "local" | "strip"
local_only = []                 # path prefixes never sent to peers
sensitive = []                  # path prefixes: no blob retention on peers
retain_blobs = true
cache_max_gb = 20
job_timeout_min = 30
forward_env = []
```

## 13. Observability

- `crabfarm status`: table of peers (name, OS/host, slots busy/total, speed, RTT, bandwidth), local running jobs (crate, where, elapsed), counters (remote ok, local by reason, bytes up/down, cache hits).
- Logs: `tracing`, node log in `<data>/logs/node.log` (daily rotation, 7 days), `CRABFARM_LOG=debug` for the wrapper (stderr, only when set).

## 14. Testing

- Unit tests: arg parsing/classification, argfile expansion, path mapping round trips, stderr and dep-info rewriting, framing, join-code encode/decode, blob encryption.
- Integration test (light, offline after one `cargo fetch`): start two nodes in one process on loopback ports with separate data dirs (farm create, invite, join), build `tests/fixture` (a small workspace: a lib crate depending on a proc-macro crate and a registry-free path dependency) through `crabfarm cargo build` with local slots = 0 so every eligible job runs remotely, then check the binary runs and prints the expected output, and that the remote job count is > 0. Must also run with a real stderr diagnostic (warning) and check its path is the client path.
- No load tests, no CPU-burning benchmarks on the owner PC.

## 15. Phase plan

- **P1** (this round): everything above unless marked P2/P3; Windows to Windows tested on the owner PC with two local nodes; real notebook + desktop run by the owner.
- **P2**: cross-OS jobs without proc macros (`--target` added, needs_host routing), result cache keyed by (toolchain, normalized args, env, input hashes), `crabfarm status --watch` live view, `node --detach` and autostart helpers (opt-in commands only), revocation list, persistent job connections.
- **P3**: proc-macro rebuild for the executing peer's host: peers record the job specs of proc-macro crates and their host dependency chain and replay them with `--target <peer host>`; result cached per host.
