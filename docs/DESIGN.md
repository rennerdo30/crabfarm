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
- **Join code** format: `cf1-<token>-<fp>@<host>:<port>`, where `<token>` is 16 random bytes and `<fp>` is the first 16 bytes of the inviter node's **certificate** SHA-256, both in unpadded lowercase base32 (RFC 4648 alphabet, `data-encoding` `BASE32_NOPAD` lowercased). The joiner connects with TLS, pins the inviter's cert by `<fp>` (it has no CA yet), sends `JoinRequest { token, node_name, public_key }` (the raw Ed25519 public key; there is no CSR); the inviter checks the token (single use, 10-minute expiry, constant-time compare), **generates the node id itself** (random 128-bit, never equal to an existing member's; the joiner cannot choose it), signs a node certificate with that id, and returns `JoinAccepted { node_id, node_cert, ca_cert, known_peers }`. The joiner checks that the certificate carries the returned id and its own key. `PROTO_VERSION` is 2 since this change. Only a node holding the CA key can issue invites in P1. Invites live in `farm/invites.toml` as SHA-256 hashes of the tokens (so the CLI process and the node process share them without the file holding a redeemable token). Every connection's `Hello` must carry the node id of the certificate it arrived under; only a `JoinRequest` first frame is accepted without a client certificate.
- **Transport**: TCP + rustls, **TLS 1.3 only**, ring provider, mutual authentication. Both sides require a peer certificate chaining to the farm CA (custom verifier: single-level chain, CA pinned by DER equality, validity checked, ignores hostnames). There is no plaintext or anonymous mode.
- **Local IPC**: TCP on `127.0.0.1:<ipc_port>` with a 32-byte random token from `ipc.token` in the node data directory (owner-only permissions, regenerated at each node start). The first frame on an IPC connection must carry the token; otherwise the node closes the connection.
- **At rest on the executing peer**: blobs a peer keeps for later jobs are encrypted with the *submitting* node's blob key (`blob.key`, 32 random bytes, never written on other peers; sent inside the TLS session with each job). XChaCha20-Poly1305, random 24-byte nonce per blob, the blob hash as associated data. Plaintext exists only in the job sandbox while rustc runs; the sandbox is deleted when the job ends (success or failure). Nodes may set `retain_blobs = false` and per-path `sensitive` prefixes (config) so blobs from those submissions are deleted after the job. Recommendation for the owner: BitLocker/FileVault/LUKS on every farm machine, because rustc must read plaintext sources during the job.
- **Resource limits against misbehaving peers**: a node serves at most 256 inbound connections at once and at most 8 that have not yet proved to be farm members (TLS handshake and first frame pending, or a join in progress); more are dropped at accept. A job connection with no frame in either direction for 10 minutes is closed (the executor lifts this while rustc runs; the job timeout bounds that phase). Blob reassembly accepts only full `CHUNK_SIZE` chunks before the last one, at most 256 MiB per blob, and at most 64 blobs / 1 GiB in flight per node. `invites.toml` changes run under an exclusive lock on `farm/invites.lock`. Peer-supplied text (node names, `Error.message`) is escaped and cut to 200 characters before it is logged, and `Debug` output of join codes and messages redacts tokens, blob keys and environment values.
- **Trust model**: any farm member can run arbitrary code on any other member via proc macros. Only join machines you own.

Data directory (via `directories`, `ProjectDirs::from("dev", "renner", "crabfarm")`):
`config.toml`, `farm/ca.crt`, `farm/ca.key` (creator only), `node.key`, `node.crt`, `node.id`, `blob.key`, `ipc.token`, `ipc.port`, `listen.addr` (bound peer address, written by the node), `peers.toml` (learned peers), `cache/` (blob store), `jobs/` (sandboxes; emptied at node start), `logs/node.log`, `depgraph.bin` and `hashcache.bin` (node caches), `rustc-id/` (wrapper cache of `rustc -vV` per compiler), `env_forward.toml` (variables learned per crate, section 9). `CRABFARM_HOME` replaces the whole directory.

## 4. Discovery, membership and load balancing

- **mDNS** (`mdns-sd`): service type `_crabfarm._tcp.local.`, instance name = node id, TXT: `f=<short farm id>`, `v=<protocol version>`. Nodes ignore other farms. mDNS is unauthenticated multicast and the farm id in `f=` is readable by anyone on the LAN, so its input is bounded: a queue of 64 events (dropped when full), candidates deduplicated by (instance, addr) and capped at 64 (a new one replaces the oldest candidate that never handshook as a farm member), at most one connection attempt per candidate per 30 s and 4 new candidate connects per second overall. A candidate counts only when its TLS handshake yields the node id it announced.
- **Static peers**: `config.toml` `peers = ["desktop.tailnet:47070", "10.0.0.5:47070"]` for VPN/remote use.
- **Gossip**: on each connection, both sides exchange `PeerList` (node id, name, addresses, last seen). Learning one address is enough to find the whole farm. Learned peers persist in `peers.toml`.
- **Status**: each node keeps a TLS control connection to every reachable peer and receives `Status` pushes every second while either side has active work, every 10 s otherwise:
  `Status { node_id, host_triple, slots_total, slots_busy, queue_len, speed, toolchains: [ToolchainId], targets: [triple], accepting }`.
  `speed` is a relative per-slot throughput score (reference core = 100). **P1: the constant 100 for every node; the benchmark is P2.** `Status` is pushed immediately when a node's busy count changes, in addition to the 1 s / 10 s timer.
- **Control connections**: both nodes may dial each other; duplicates are harmless and a dialer drops its new connection when one to that node already exists. A control connection with no frame (status pushes included) for 60 s is closed; dialers reconnect with exponential backoff (1 s up to 60 s). A node advertises itself in `PeerList` with the addresses of its `listen` socket (a wildcard expands to the non-loopback IPv4 addresses of the machine).
- **Job connection**: a separate TLS connection per job. After `Hello` its first message is `Reserve`, which the executor answers with `Reserved` or `Busy`; then `JobRequest` follows. A control connection starts with `PeerList` or `StatusUpdate`.
- **Scheduling** (on the submitting node, per job):
  1. Candidates are peers that are reachable, `accepting`, and can run the job now: toolchain installed (or installable, see section 7: an installing peer is not a candidate for that job), target installed, and (proc-macro jobs or jobs with `--extern` proc macros) `host_triple == submitter host_triple` until P3.
  2. Local slots count as a candidate (the local node itself) with link cost 0.
  3. Score = `free_slots * speed - link_penalty(upload_bytes)`, where `link_penalty` is one reference slot (100 points) per second of upload at the measured peer bandwidth (EWMA over uploads of at least 2 MiB; default 100 Mbit/s until measured). `upload_bytes` counts the inputs not yet sent to that peer (the submitter remembers what it sent; peers keep blobs). `free_slots` of a peer is its reported free slots minus the reservations this node is about to make; a reservation stops counting once the peer confirmed it. Pick the max; ties go local. Peers without a free slot are never chosen. Local has `free local slots * 100` and link cost 0. Config `prefer_remote = true` picks the best peer with a free slot even when local slots are free.
  4. A slot is reserved on the chosen peer with `Reserve` (on the job connection); if the peer refuses (full) or is unreachable, retry the next candidate once, then run locally. Input collection (the expensive step) only happens after a first scoring without upload cost says a peer beats local.
- **Backpressure**: a node never queues more remote jobs than `slots_total`; it refuses with `Busy`. `slots` (serving capacity) and `local_slots` (concurrent local rustc runs, default logical CPUs) are separate. Its own local wrappers get a `RunLocal` permit when a local slot is free, else they wait in a FIFO.
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

The wrapper only handles a program whose file stem is `rustc`. `clippy-driver` (or any other driver behind `RUSTC_WRAPPER`) always runs locally: the peers have no such driver, and a remote `rustc` run would lint nothing.

The executing peer does not trust the wrapper (CF-8): `guard.rs` re-checks the whole argument list with the same flag parser as the classifier and refuses the job (`Error`, the submitter runs locally) when a path-carrying flag (`--out-dir`, `-o`, `-L`, `--extern name=`, `--emit kind=`, `-C linker=`) holds anything but a structured path or a safe relative path (no root, drive, `..`), or when the list contains `@file`, `-Z`, `--sysroot`, `--print`, `-C incremental`, `-C ar`/`dlltool`/`save-temps`/`profile-*`/`target-cpu=native`, crate types other than `lib`/`rlib`/`proc-macro`, an `--emit` kind outside the allowed set, a second input, or an unknown flag with an absolute path attached.

Target: if no `--target` is given, the wrapper adds `--target <client host triple>` for the remote run (a remote of another OS would otherwise build for its own host). Cargo's sysroot detection is not affected because the explicit target equals the default.

## 6. Paths

- `ClientPath`: an absolute, normalized path string as seen on the submitting machine plus its style (`Windows` or `Posix`). Windows paths: drive letter uppercased, `\\?\` prefix removed, separators `\`.
- Sandbox mapping on the executing peer, job root `R = <data>/jobs/<job id>/root`:
  - Windows `C:\a\b` → `R/C/a/b`; UNC `\\server\share\a` → `R/UNC/server/share/a`.
  - Posix `/a/b` → `R/_/a/b`.
  - Mapping is a pure function in both directions; tests cover round trips.
  - Every component of a client path is validated in `ClientPath::parse` and again in `SandboxMap::to_sandbox` (CF-7): no `:`, `\`, `/`, control characters or `<>"|?*`, no trailing dot or space, no `.`/`..`, no reserved device names (`CON PRN AUX NUL COM1-9 LPT1-9`, with or without an extension). The strictest (Windows) rules apply to both styles because a Posix client path is also mapped on Windows peers. A path that fails is not remote-eligible (the wrapper runs locally); on the peer `to_sandbox` returns `None` and the job is refused. The sandbox path is built from validated single names and checked to lie strictly below the job root.
- Relative arguments stay relative; rustc runs with `cwd` = mapped client cwd.
- `--remap-path-prefix`: for each drive/root that appears in the job, add `--remap-path-prefix <mapped root>=<client root>` **before** the user's own remap flags (rustc applies the last matching flag, so the user's flags still win).
- **stderr rewriting**: every stderr line (JSON diagnostics and rendered text) has mapped prefixes replaced by client prefixes; for a Windows client, `/` after a replaced prefix in the same path token is converted to `\` (best effort; JSON strings are rewritten after parsing, not by raw text replace).
- **dep-info (`.d`) rewriting**: output `.d` files are rewritten to client paths before they are written locally (cargo fingerprints from them). Escaped spaces (`\ `) are preserved.
- Environment values that start with a mapped job path are mapped (`OUT_DIR`, `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH`, `CARGO_TARGET_TMPDIR`, `CARGO_BIN_EXE_*`, `CARGO_RUSTC_CURRENT_DIR`).

## 7. Toolchains

- `ToolchainId { release: String, commit_hash: String, host: String }` from `rustc -vV` (`release`, `commit-hash`, `host`). The executing peer must have a toolchain with the same `release` and `commit_hash`; its own `host` may differ (P2/P3).
- The submitting node caches `rustc -vV` per rustc path + file size + mtime.
- Executing peer resolves the toolchain via `rustup toolchain list` and `rustc +<name> -vV`, cached. Missing: if `auto_install = true` (default), run `rustup toolchain install <spec> --profile minimal --no-self-update` in the background, where `<spec>` = `release` for stable/beta, `nightly-<commit-date + 1 day>` for nightly (verified by commit hash after install). Missing target: `rustup target add --toolchain <name> <target>`. While installing, the peer reports the toolchain as not installed, so jobs run elsewhere. The toolchain id and target come from a peer and are validated before any rustup call: release `^\d+\.\d+\.\d+(-beta\.\d+|-nightly)?$`, commit date `YYYY-MM-DD` and an existing date, commit hash 40 lowercase hex, target `^[A-Za-z0-9_.-]{1,64}$` not starting with `-`; anything else is refused without a download. At most `max_auto_toolchains` (default 4) toolchains are installed by the node itself (the names are kept in `auto-toolchains.toml` in the data directory); a further request is refused. If the installed build's commit hash does not match, the node runs `rustup toolchain uninstall <name>` for the toolchain it just added (only that one) and remembers the (spec, target) as failed; the failed set keeps the 64 most recently used entries.
- Rustup child processes have a 30-minute timeout and are recorded by PID; only those are ever killed.

## 8. Blob store and inputs

- Content addressing: BLAKE3 of the plaintext, 32 bytes. Transfer: zstd level 3, 1 MiB chunks.
- **Inputs of a job**:
  1. Source files: all regular files under the package directory (`CARGO_MANIFEST_DIR`, or the input file's directory when not set), skipping `target/`, `.git/`, any directory starting with `.`, and files over 64 MiB. Hard cap: 256 MiB total, else local.
  2. `OUT_DIR` contents (recursively) if set.
  3. Dependency libraries: every file passed with `--extern`, plus the **transitive closure** that rustc finds through `-L dependency=` (it needs the metadata of indirect dependencies, and proc macros re-exported by a dependency, e.g. `serde` re-exporting `serde_derive`). The closure comes from the node's **dependency graph** (`depgraph.rs`): for every rustc invocation that goes through the wrapper and is remote-eligible (local or remote), the node records `produced library file → its --extern files` (keyed by client path, with size and mtime). The entry is created as *pending* when the wrapper submits and stamped when the invocation ends, so a pipelined dependent that starts when the metadata is announced already finds it; a pending entry older than 30 minutes is unknown. A failed invocation drops its entries. Invocations the wrapper classifies as local without contacting the node (for example debug builds with incremental) are not recorded, so their dependents run locally. Closure = transitive walk of that map from the job's `--extern` files. If any file in the walk is unknown (built before crabfarm was used) or changed since it was recorded, the job runs locally (and is recorded). If the closure contains a proc-macro library, the job gets `needs_host = true`.
  4. Native libraries: for each `-l [static=]name`, the file `name.lib` (msvc) or `libname.a` (others) found in `-L native=`/`-L all=`/`-L` directories.
- The submitting node caches file hashes by (path, size, mtime) so unchanged files cost no hashing.
- **Have/need**: the submitter sends the manifest (path, hash, size, executable); the peer answers `NeedBlobs [hash]` for blobs not in its store; the submitter streams those. Outputs a peer produced stay in its store (encrypted with the submitter's key), so dependency rlibs are almost never uploaded.
- **Store**: `cache/<first 2 hex>/<hash hex>`, encrypted (section 3). LRU eviction above `cache_max_gb` (default 20). The input blobs of a running job are pinned from the moment its `JobRequest` arrives until it ends: `evict` skips them and a `remove` (the `no_retain` cleanup of another job) is deferred until the last pin is released.

## 9. Running a job on a peer

1. Resolve toolchain; ensure target.
2. Materialize the sandbox: decrypt each input blob into its mapped path (hard links are not used; files are written). Create mapped `--out-dir` and `OUT_DIR`.
3. Build the argument list (mapped paths, added `--target`, added remaps, `-C incremental` stripped if configured), environment (forwarded variables from the job, mapped paths), `cwd`.
4. Spawn `rustc` of the resolved toolchain (direct path to the toolchain's `bin/rustc`, not the rustup proxy) with a node-wide **jobserver** (`jobserver::Client::new(slots_total)`, passed with `configure`) so codegen threads of all jobs share the peer's cores. Timeout: `job_timeout_min` (default 30). rustc runs inside a process tree the node owns (CF-9): on Windows a Job Object with `KILL_ON_JOB_CLOSE`, on Unix its own process group killed as a group. Timeout, cancel and errors end the whole tree, so a linker started by rustc cannot outlive the job and lock sandbox files. If the tree cannot be tracked, the job is refused.
5. Stream stderr line by line to the submitter, rewritten (section 6). When a line is an artifact notification (`{"artifact":"<path>","emit":"metadata"}` etc.), send that file as an `Output` first, then the line, so the submitter writes the file before cargo sees the notification (pipelining).
6. On exit: send every new or changed file in the mapped out-dir (rewriting `.d` files), then `Exit { code }`. Store outputs in the blob store under the submitter's key. Delete the sandbox. The sandbox directory is `jobs/<owner node id>-<job id>`; a request whose (owner, job id) is already running on the node is refused and never touches the running job's directory. If the directory cannot be removed at once (locked file), removal is retried on a background thread with growing delays (1 s doubling to 30 s, 20 attempts, logged) until it is gone; `jobs/*` is swept the same way when the node starts.

Caps per job, checked before anything is written: at most 100 000 input files, 10 000 directories and 512 MiB of materialized input (declared sizes up front, real sizes again before each write). `-C linker=<absolute path>` is kept on the peer only when the host triples are equal and a known linker program (`link`, `lld-link`, `rust-lld`, `ld`, `lld`, `cc`, `gcc`, `clang`, `<triple>-gcc`, ...) exists at exactly that path outside the job sandboxes; it is then passed as that native path. Otherwise the flag is dropped and rustc uses its default linker.

Environment accepted by the executor (CF-8): the fixed list below, plus the names the job declares in `JobSpec.extra_env` (the submitter's `forward_env` and the names it learned per crate). `PATH`, `RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER`, `RUSTFLAGS`, `LD_*`, `DYLD_*`, `RUSTUP_*`, `MAKEFLAGS` and machine variables (`SystemRoot`, `HOME`, `TEMP`, ...) are never accepted, even when declared, and a declared name may not override a variable of the peer's own environment; any other undeclared name refuses the job. The submitter never sends them either (`forward_env = ["PATH"]` is ignored). `RUSTC_BOOTSTRAP` is accepted because the list below forwards it and it only reaches a job when the submitter itself had it set; the peer's own value never reaches a job.

Environment forwarded from the wrapper: all `CARGO_*`, `OUT_DIR`, `RUSTC_BOOTSTRAP`, `RUSTFLAGS`-derived nothing (cargo already expanded them into args), `DEP_*`, `TARGET`, `HOST`, `PROFILE`, `NUM_JOBS`, `OPT_LEVEL`, `DEBUG`, plus names listed in the config `forward_env`. After a remote success, the wrapper parses the `# env-dep:` lines of the returned `.d` (`<out-dir>/<crate name><extra-filename>.d`): if a listed variable is set locally but was not forwarded, the job is rerun locally (`RetryLocal`) and the name is remembered per crate in `<data>/env_forward.toml` (written by the wrapper) so later runs forward it. Variables holding paths (`OUT_DIR`, `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH`, `CARGO_TARGET_TMPDIR`, `CARGO_RUSTC_CURRENT_DIR`, `CARGO_BIN_EXE_*`) are sent as client paths and mapped on the peer; `CARGO_MAKEFLAGS` is never forwarded. A crate that embeds such a path with `env!` would carry the sandbox path (CF-10): the executor scans every output file (after `.d` rewriting) for the sandbox root as bytes, in the host spelling, the other separator spelling and the `\\?\` prefixed form (UTF-16 is not searched). Debug-symbol `.pdb` files are skipped: they list the link's object paths by design and nothing loads them. On a hit it stores nothing of the job, sends `Tainted { path }` instead of that output and stops rustc. The submitter stages every output next to its destination (temporary file) and applies it at `Exit` (renamed), so a tainted job leaves nothing in the target directory; it discards the staged files and runs the job locally, counted as `local.SandboxPathInOutput`.

**Pipelining (CF-16).** The executor scans each artifact file when its notification arrives, with the same scan, and sends a clean file before the notice line; a tainted one is never sent. The submitter moves a staged `metadata` or `dep-info` file to its client path (atomic rename) when its notice arrives and relays the notice at once, for registry and path crates alike (`early_notices` counter, shown by `crabfarm status`). The wrapper prints these two notice kinds immediately even when it buffers the diagnostics of a path crate (section 10), so cargo starts dependents of a remote crate while its codegen still runs. All other artifacts (`link`, `obj`, ...) stay staged until `Exit`; from the first such notice on the submitter holds every later frame until `Exit` (files first, then the held lines, then `Exit`), which keeps the order of diagnostics. A notice whose file is not staged, or is not an early kind, is held the same way: cargo never sees a notice before its file exists. A taint found later (for example in the `.rlib`) discards the staged files and the held lines and reruns the job locally; the early `.rmeta` stays and the local run overwrites it. That is safe because a clean `.rmeta` passed the same scan: it holds no sandbox path and is what a local build produces (same inputs, same flags), so dependents that already read it are not affected by the taint. The overwrite is an atomic rename by rustc. A dep-info that lists an env variable which was set locally but not forwarded makes the wrapper stop relaying early notices for that job (its metadata could differ), and the job is rerun locally at `Exit` (section 9, `RetryLocal`); this relies on the dep-info being written before the metadata, which rustc does; if it ever arrived later, the existing check at `Exit` still reruns the job but dependents may have used the early metadata.

## 10. Results and fallback

- Remote exit code 0: outputs are written to the client paths (atomic rename per file), stderr was already forwarded, wrapper exits 0.
- Remote nonzero exit with diagnostics: the error is authoritative for registry crates (path under `CARGO_HOME`) and forwarded as is. For other crates (workspace, git, path deps), the wrapper reruns locally with a clean stderr: the remote stderr is buffered, not forwarded, for these crates, and printed only when the remote result is used (this costs at most one duplicate compile on a real error, and makes missing-input problems invisible to the user). The wrapper asks for the local run with `RetryLocal`; the node answers `RunLocal` once a local slot is free. The buffering covers diagnostics only: `metadata` and `dep-info` artifact notices are printed at once (CF-16, section 9), so pipelining works for path crates too. When such a job is rerun locally (this case, a taint, an env dependency, an infrastructure failure), the local rerun's stderr is filtered: artifact notices cargo already saw from the remote attempt (same kind and file name) are dropped, because a repeated metadata notice would release dependents twice and cargo does not expect that. The rerun's other notices (the `link` one) and diagnostics pass unchanged. A remote compile error after an early metadata (rare) therefore reruns locally like before; if the local rerun fails too, cargo has a failed unit whose metadata dependents may already have read, as in a native pipelined build.
- **Staged outputs and tainted jobs**: see section 9 (CF-10). `Message::Tainted` and `LocalReason::SandboxPathInOutput` were appended to the protocol.
- **Output paths**: an `Output` must lie under the job's `out_dir` (or be one of the files the invocation announced as products), must be a normalized absolute path of the client's style, and its size and hash must match; otherwise the job is abandoned and runs locally. Every file is written to a temporary name in its directory and renamed, before any later frame (such as the artifact notice) is passed on to the wrapper.
- Infrastructure failure at any point before the first stderr line was forwarded: rerun locally. After stderr began (registry crates): the wrapper buffers nothing more; it reports `crabfarm: remote job failed (<reason>), rerunning locally` once to stderr and reruns.
- Every fallback is counted per reason in node stats.

## 11. Wire protocol

- Frame: `u32` little-endian length + postcard-encoded `Message`. Max frame 16 MiB. Blob chunks are `BlobChunk { hash, offset, last, data }` frames.
- Every connection starts with `Hello { proto: u16, node_id, node_name, host_triple, crabfarm_version }` from both sides; a different `proto` closes the connection with `Error`.
- Job connection (P1: one TLS connection per job, rustls session resumption on): `JobRequest` → (`NeedBlobs` ↔ `BlobChunk`*) → `Started` → (`Stderr` | `Output` + `BlobChunk`*)* → `Exit`. Either side may send `Cancel`/`Error`.
- IPC (wrapper to node): `IpcAuth`, then `Submit` (spec, products, externs, plus the primary input, `-L` dirs and `-l` libraries the node needs to collect inputs). The node answers `RunLocal` (the wrapper runs rustc, then sends `LocalDone`; the connection stays open meanwhile and holds the local slot) or relays `Stderr`/`Stdout`/`Exit`. After an `Exit` the wrapper may send `RetryLocal { reason }` (section 10). `QueryStatus` is answered with `FarmStatus`, whose counters are `(name, value)` pairs (`remote_ok`, `remote_failed`, `remote_exit_nonzero`, `local.<Reason>`, `bytes_up`, `bytes_down`, `served`, `running_local`, `running_remote`, `local_slots`).
- Message types are defined in `src/proto.rs`; it is the single source of truth.

## 12. Configuration (`config.toml`)

```toml
node_name = "desktop"          # default: hostname
listen = "0.0.0.0:47070"
slots = 32                      # default: logical CPUs
accept_remote_jobs = true
peers = []                      # static peer addresses
auto_install = true
max_auto_toolchains = 4         # toolchains this node installs by itself for peers
remote_incremental = "local"    # "local" | "strip"
local_only = []                 # path prefixes never sent to peers
sensitive = []                  # path prefixes: no blob retention on peers
retain_blobs = true
cache_max_gb = 20
job_timeout_min = 30
forward_env = []
local_slots = 16                # concurrent local rustc runs; default: logical CPUs
mdns = true                     # tests and fixed setups set false
prefer_remote = false           # true: eligible jobs go to a peer with a free slot even if local slots are free
```

## 13. Observability

- `crabfarm status`: table of peers (name, OS/host, slots busy/total, speed, RTT, bandwidth), local running jobs (crate, where, elapsed), counters (remote ok, local by reason, bytes up/down, cache hits).
- Logs: `tracing` to stderr and appended to `<data>/logs/node.log` (**rotation is P2**), level from `CRABFARM_LOG` (default `info`; the wrapper prints its local-classification reasons to stderr when `CRABFARM_LOG` is set). Info level logs crate names, sizes and timings, never stderr contents, environment values, keys or tokens.
- `status` also shows peers that are known but offline, and a one-line summary first.

## 14. Testing

- Unit tests: arg parsing/classification, argfile expansion, path mapping round trips, stderr and dep-info rewriting, framing, join-code encode/decode, blob encryption.
- Integration test (`tests/e2e_two_nodes.rs`, light, offline): two nodes as child processes of the built binary on `127.0.0.1` ports (mDNS off, separate `CRABFARM_HOME`s; farm create, invite, join), then `crabfarm cargo build --release` of `tests/fixture` (`app` bin, `core_lib` with a warning, `derive_mac` proc macro, `util` with a build script and a warning inside `OUT_DIR`; no registry crates; copied to a temp dir). Node one has `local_slots = 1` and `prefer_remote = true`. It checks that the binary prints the expected text, that at least three jobs ran remotely, that warnings show client paths and no sandbox paths, that a second build recompiles nothing (dep-info), and that a compile error is reported with client paths, a nonzero exit and a local rerun. Note: cargo 1.100's build layout puts each unit's intermediate files in `target/<profile>/build/<crate>/<hash>/out`, so the `--out-dir` is that directory, not `deps/`. rustc also applies the `--remap-path-prefix` flags the executor adds to its own diagnostics, so the executor's stderr rewriting is a second layer: the test fails when `.d` rewriting is off, and when stderr rewriting and the remap flags are both off.
- No load tests, no CPU-burning benchmarks on the owner PC.

## 15. Phase plan

- **P1** (this round): everything above unless marked P2/P3; Windows to Windows tested on the owner PC with two local nodes; real notebook + desktop run by the owner.
- **P2**: cross-OS jobs without proc macros (`--target` added, needs_host routing), result cache keyed by (toolchain, normalized args, env, input hashes), `crabfarm status --watch` live view, `node --detach` and autostart helpers (opt-in commands only), revocation list, persistent job connections.
- **P3**: proc-macro rebuild for the executing peer's host: peers record the job specs of proc-macro crates and their host dependency chain and replay them with `--target <peer host>`; result cached per host.
