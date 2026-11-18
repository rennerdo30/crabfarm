# CF-5 Package D: node daemon, scheduler, IPC, wrapper flow, CLI, end-to-end test

Read `docs/briefs/common.md` first; it is part of this brief (except: base and branch are below, and you are the only implementation job now, so `jobs = 8` is fine). Pair Desk issue: `CF-5`. Base: branch `cf-p1-abc` at the commit named in your task message (packages A, B, C merged; read their reports summarised at the end of this file). Branch: `cf-5-node`.

**Goal:** a working P1: `crabfarm farm create|invite|join`, `crabfarm node`, `crabfarm cargo ...`, `crabfarm status`, and wrapper mode, per `docs/DESIGN.md`. A parallel review of A+B+C is running; its findings may be fixed forward later, so do not refactor A/B/C modules beyond what integration needs (list every change).

## Modules to add (names are suggestions; keep `src/` flat)

- `depgraph.rs`: DESIGN section 8 item 3. Persisted map `produced library file (client path) → (size, mtime, extern files)`; `closure(externs) -> Result<Vec<ClientPath>, Unknown>`; records every invocation (local or remote) from `Submit`/`LocalDone`.
- `ipc.rs`: loopback server on `127.0.0.1:0` (port written to `ipc.port`), token in `ipc.token` (32 random bytes, regenerated each node start). First frame must be `IpcAuth`.
- `scheduler.rs`: DESIGN section 4. P1 simplification: `speed` is a constant 100 for every node (no benchmark), so the score is free slots minus link penalty. Respect `needs_host` (same host triple), toolchain `same_compiler`, installed target.
- `node.rs`: the daemon.
  - Peer listener on `config.listen`; `transport::accept` → `Accepted::Peer` (control or job connection: first message after Hello decides) or `Accepted::Join` (`farm::handle_join`).
  - Control connections to every known peer (PeerBook + static `peers` + mDNS when `config.mdns`), `PeerList` gossip, `StatusUpdate` every 1 s while busy else 10 s, reconnect with backoff.
  - Executing side: `Reserve`/`Reserved`/`Busy` slot accounting; job connection: `JobRequest` → `NeedBlobs` → receive `BlobChunk`s into the store → `toolchain.resolve` (None → `Error`, submitter falls back) → `exec::run` → frames (`Started`, `Stderr`, `Stdout`, `Output` + chunks, `Exit`).
  - Submitting side: handle `Submit` from a wrapper: build the full `JobSpec` inputs (`inputs::collect` + `HashCache` + `depgraph::closure` + produced-library check), apply DESIGN section 10 rules (`local_only`, `sensitive` → `no_retain`, too large → local), pick a peer or answer `RunLocal`, run the remote job, answer `NeedBlobs` from local files, write every `Output` atomically to its client path (temp + rename) BEFORE relaying any later frame to the wrapper, relay `Stderr`/`Stdout`/`Exit` to the wrapper.
  - Local slots: `config.local_slots` (new, default logical CPUs) separate from `config.slots` (serving capacity). `RunLocal` permits are FIFO.
  - Counters for `crabfarm status` (remote ok, local by `LocalReason`, bytes up/down).
  - Logging: `tracing-subscriber` to stderr and appended to `<data>/logs/node.log` (rotation is P2; update DESIGN section 13 to say so).
- `wrapper.rs`: DESIGN sections 2, 5, 9 (env forwarding), 10. Classify with `rustc_args`; local passthrough when not eligible or the node is not reachable (`ipc.port` missing or connect refused within 200 ms); otherwise `Submit`. On `RunLocal`: run rustc with inherited stdio, send `LocalDone`. On a remote run: stderr lines go to the wrapper's stderr in order. For non-registry crates (not under `CARGO_HOME`), buffer remote stderr and only print it if the remote result is used; on a remote compile error rerun locally. Registry crates: remote errors are authoritative. `# env-dep:` check after success (rerun locally when a listed variable is set locally but was not forwarded; remember it per crate). Exit code = rustc's.
- `cargo_cmd.rs`: `crabfarm cargo <args>`: run the `cargo` on PATH with `RUSTC_WRAPPER=<this exe>`; if the user gave no `-j`/`--jobs`, add `-j` = local_slots + free remote slots (QueryStatus), cap 128; when the node is not running, print one line saying the build runs locally and how to start the node; propagate cargo's exit code. Insert `-j` after the cargo subcommand (e.g. `build -j N ...`), not before it.
- `status.rs`: `crabfarm status` table (peers: name, host, slots busy/total, last seen; counters).
- `main.rs`: CLI wiring. `farm invite` prints the join code with `--addr <host:port>` or, by default, the first non-loopback IPv4 of this machine plus the listen port (if-addrs is already in the tree through mdns-sd; add it as a direct dependency if needed). `farm join` then prints what to run next.

## Config additions (update DESIGN section 12 and `config.rs`)

- `local_slots` (default logical CPUs).
- `mdns = true` (tests set false).
- `prefer_remote = false`: when true, eligible jobs go to a peer with a free slot even when local slots are free (tests and users who want to spare a notebook battery).

## Safety (from AGENTS.md; each is a review attack class)

- Tests bind only `127.0.0.1` and set `mdns = false`: binding `0.0.0.0` or multicast on the owner's PC can open a Windows Firewall dialog on the owner's desktop. Never do that in tests or manual runs.
- The node spawns only rustc (via exec) and rustup (via toolchain); tests spawn nodes and cargo as child processes with recorded PIDs, kill only those, with timeouts (whole e2e test ≤ 5 minutes).
- A malicious peer must not make a submitter write outside the paths the submitter expects: `Output` paths must be under the job's `out_dir` (or be the produced files), else the job fails and runs locally.
- Never log stderr contents, env values, keys or tokens at info level; debug level may log crate names and paths.

## End-to-end test (`tests/e2e_two_nodes.rs`)

- Fixture workspace under `tests/fixture/` (committed): `app` (bin) depends on `core_lib` (lib with a deliberate `#[warn(unused)]` warning in it) which depends on `derive_mac` (proc-macro, no external deps: use `proc_macro` only) and `util` (lib with a build script that writes a generated `.rs` file to `OUT_DIR`, included with `include!`). No registry crates, so the test runs offline.
- Two nodes as child processes of the built binary (`env!("CARGO_BIN_EXE_crabfarm")`), separate `CRABFARM_HOME` tempdirs, `listen = "127.0.0.1:0"`-style fixed free ports, `mdns = false`, node 1 `local_slots = 1`, `prefer_remote = true`; farm create on node 1, invite, join from node 2.
- Run `crabfarm cargo build --release` (no incremental, so path crates are eligible) for the fixture with `CRABFARM_HOME` of the submitting node, in a fresh target dir. Assert: exit 0; the app binary runs and prints the expected text (proving proc macro + OUT_DIR include worked); the status counters show remote ok ≥ 3; the `core_lib` warning appears in cargo's output with the client path of the source file (not a sandbox path).
- Run once more with a deliberately broken copy of a crate (compile error) and assert the error is reported with client paths and nonzero exit.
- Run the same build again: cargo says everything is fresh (dep-info rewriting works).

## Acceptance (finite list)

1. All modules above implemented; `cargo clippy --all-targets` clean; unit tests for scheduler scoring, depgraph closure (including unknown and changed files), output path containment, cargo `-j` insertion.
2. The e2e test passes on this machine; show once that it fails when output `.d` rewriting is disabled or when stderr rewriting is disabled.
3. README "How it will work" becomes "Quick start" with the real commands, plus a "Windows Firewall" note (allow `crabfarm` on private networks for port 47070 when the node first starts).
4. DESIGN updated for every behaviour you changed or decided.
5. Branch pushed; final report per common brief.

## Reports from packages A, B, C (summary)

- A: `SandboxMap::render(&ClientPath)` gives the sandbox path with host separators; use it for every sandbox path in args/env. `Rewriter::new(pairs, client_windows)`. `classify` emits `--remap-path-prefix` as `Plain` + `RemapFrom`. Relative `--out-dir`/`--extern`/`-L`/input stay `Plain` in args; absolute resolved values are in the struct fields. `-C linker=<absolute>` stays a `PrefixedPath`: drop it on the peer if the file is missing or the host differs. No `--crate-type` → Local. Debug builds of path crates are Local (incremental) unless `remote_incremental = "strip"`.
- B: `transport::accept` returns `Accepted::Peer { node_id, hello, conn }` / `Accepted::Join { request, conn }`; `farm::handle_join(id, &mut Invites, data_dir, &Message)`. Set `TlsContext.host_triple` to the real rustc host triple. `invites.toml` stores token hashes; max 32 pending.
- C: `exec::run` errors carry `ExecAborted { Timeout, Cancelled }` for downcasting; nonzero rustc exit is `Exit(code)`, not an error; outputs only on exit 0. `Toolchains` is `Clone`, has `rescan`, `is_installing`, `shutdown`. rustc's artifact notifications cover `.d`, `.rmeta`, `.rlib`; none are rewritten afterwards.
