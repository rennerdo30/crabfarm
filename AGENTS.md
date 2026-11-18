# crabfarm agent rules

Read `E:/Development/AGENTS.md` and `E:/Development/general-agent-rules.md` first; they hold the hard stops and shared rules. This file adds the project's own rules.

## Project

- crabfarm spreads rustc work over a peer-to-peer farm of Windows, macOS and Linux machines. `docs/DESIGN.md` is the specification; change it in the same commit as any behaviour it describes.
- `src/proto.rs` is the single source of truth for wire messages. Any incompatible change bumps `PROTO_VERSION`.
- Pair Desk project: `crabfarm` (issues `CF-n`). Every change belongs to an issue.

## Build and test

- Machine-local `.cargo/config.toml` (from `.cargo/config.example.toml`): shared sccache, rust-lld, output in `E:/Development/_cache/cargo-target/crabfarm`, `jobs = 8`. Never commit `.cargo/config.toml`.
- Toolchain pinned in `rust-toolchain.toml`.
- Checks: `cargo build`, `cargo clippy --all-targets` (no warnings), `cargo test` (unit tests plus the light two-node loopback integration test; no load tests, no benchmarks that burn CPU).
- Tests must be able to fail: show a new test failing on broken code before keeping it.

## Safety rules

- Every byte between machines goes over mutually authenticated TLS 1.3. No plaintext, anonymous or "insecure" mode, not even for tests (tests create a throwaway farm).
- Never log job contents, environment values, keys, tokens or join codes. Log crate names, sizes, hashes (shortened) and timings only.
- A remote result is used only when it is known to be equivalent to a local build; when in doubt, run locally (DESIGN section 10). Fallbacks are counted, never silent in stats.
- The node kills only processes it spawned and recorded (rustc jobs, rustup installs). Every child has a timeout.
- No tool command changes persistent environment, autostart or services unless the user runs an explicit opt-in command for it (P2).
