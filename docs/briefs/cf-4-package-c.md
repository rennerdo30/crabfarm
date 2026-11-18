# CF-4 Package C: encrypted blob store, toolchains, job executor

Read `docs/briefs/common.md` first; it is part of this brief. Pair Desk issue: `CF-4`. Branch: `cf-4-executor`.

**Goal:** implement `src/cas.rs`, `src/toolchain.rs`, `src/exec.rs` per DESIGN sections 3, 7, 8 and 9 and the stub doc comments.

**Dependency on a sibling:** `exec.rs` uses `paths::SandboxMap`, `rustc_args::render_args`, `rewrite::Rewriter` and `rewrite::artifact` from CF-2 (branch `cf-2-wrapper-logic`, being written in parallel; it pushes those first). Do `cas.rs` and `toolchain.rs` first. Then write `exec.rs` against those documented contracts; to run the exec tests, `git fetch origin` and merge `origin/cf-2-wrapper-logic` into your branch once it has them (a merge commit is fine; do not edit CF-2's files; if you find a bug in them, describe it in your report instead of fixing it).

**cas.rs**
- File layout: `cache/<owner node id hex>/<2 hex>/<64 hex>`, content `b"CFB1"` + 24-byte nonce + XChaCha20-Poly1305 ciphertext (`chacha20poly1305` 0.11, AAD = the 32 hash bytes). Atomic write (temp file in the same dir + rename). `get` verifies the plaintext hash after decryption.
- LRU: mtime touched on get/put; `evict` deletes oldest until under `max_bytes`.
- `chunks`/`Assembler`: zstd level 3 per 1 MiB chunk; Assembler rejects out-of-order offsets, oversize totals (> 1 GiB) and hash mismatch.

**toolchain.rs**
- Parse `rustc -vV` (stable, beta and nightly samples in tests).
- `scan`: `rustup toolchain list` (timeout 30 s); for each toolchain the direct rustc is `<rustup home>/toolchains/<name>/bin/rustc(.exe)` where rustup home is `RUSTUP_HOME` or `~/.rustup`; run it with `-vV`; installed targets from `rustup target list --installed --toolchain <name>`. Cache results in `<data>/toolchains.toml` keyed by the toolchain dir mtime.
- `resolve`: exact `same_compiler` match plus installed target. When missing and `auto_install`: spawn ONE background install at a time per spec (`rustup toolchain install <spec> --profile minimal --no-self-update`, then `rustup target add`), 30-minute timeout, PID recorded, killed only by that record. After install, verify the commit hash; a mismatch is logged and the toolchain is not used. **Tests must not install toolchains or targets** (network, disk): test `install_spec`, parsing, and `scan` against the machine's real rustup read-only.

**exec.rs**
- `NodeJobserver::new(slots)` creates `jobserver::Client::new(slots)`; each rustc gets it via `client.configure(&mut std_cmd)` before converting to `tokio::process::Command`.
- Sandbox root `<jobs_dir>/<job id hex>/root`; materialize inputs from the store (`spec.inputs`), create `spec.dirs`.
- Args: `render_args(&spec.args, |p| sandbox path string)`, with `--remap-path-prefix <sandbox root>=<client root>` pairs from `SandboxMap::remaps` inserted **before** the first user arg (rustc applies the last matching remap, so the user's own remaps still win).
- Environment: start from the node's own environment, remove build-related variables (`CARGO*`, `RUSTC*`, `RUSTDOC*`, `RUSTFLAGS`, `OUT_DIR`, `DEP_*`, `TARGET`, `HOST`, `PROFILE`, `NUM_JOBS`, `OPT_LEVEL`, `DEBUG`, `MAKEFLAGS`, `MFLAGS`, `CARGO_MAKEFLAGS`), then add `spec.env` (paths mapped). Rationale: rustc and the MSVC linker need the system variables; the job's build variables come only from the submitter.
- cwd = mapped `spec.cwd`.
- stderr: read line by line, rewrite via `Rewriter`; if the line is an artifact notification, read that file, emit `Output` for it, then the `Stderr` line. stdout lines as `Stdout`.
- On exit: outputs = files in the mapped out-dir that are not inputs or whose hash differs from the input with the same path, excluding files already emitted as artifacts unless they changed afterwards (rustc rewrites rmeta? verify, and document what you found). `.d` files are rewritten with `Rewriter::dep_info` before hashing and emitting. Store outputs in the blob store under (owner, key) unless `no_retain`; with `no_retain`, also remove this job's input blobs afterwards. Delete the sandbox in all cases (success, error, timeout, cancel).
- Timeout and cancel kill only the rustc child this call spawned (`kill_on_drop` plus explicit kill), and still clean up the sandbox.

**Tests**
1. cas: put/get round trip, wrong key fails, wrong owner fails, tampered file fails, eviction order, chunk round trip with a > 2 MiB blob, Assembler rejections.
2. toolchain: parsing samples; `scan` lists the machine's active toolchain (read-only).
3. exec (after merging CF-2): build a tiny lib crate entirely through `exec::run` with a hand-built `JobSpec` (client paths in a tempdir, inputs put in a tempdir store): rmeta artifact `Output` arrives before its stderr line, rlib and `.d` outputs arrive, the `.d` contains client paths only, a warning diagnostic in the source shows client paths, the sandbox dir is gone afterwards; plus a compile-error case (nonzero exit, sandbox still removed) and a timeout case using a short timeout (no busy loops; use a crate that makes rustc wait, e.g. a very large `const` evaluation is NOT allowed (CPU burn): instead test the timeout path with a fake "rustc" that sleeps, a tiny script/exe you create, killed by the timeout).

**Acceptance (finite list)**
1. Three modules implemented, no `todo!()` left in them.
2. All tests above green (exec tests need the CF-2 merge; if CF-2 is not available in time, push everything else, keep the exec tests written, and say so).
3. `cargo clippy --all-targets` clean for these modules.
4. Branch pushed; final report per common brief.
