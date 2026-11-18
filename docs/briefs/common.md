# Common brief for crabfarm P1 package jobs (CF-2, CF-3, CF-4)

- **Repository:** `E:\Development\crabfarm` (private GitHub `rennerdo30/crabfarm`). You work in your own worktree, which T3 prepared from base `main` at the commit named in your task message. First `git fetch origin` and confirm your base; report it.
- **Branch:** create the branch named in your brief and push only with `git push origin HEAD:refs/heads/<branch>` (no `-u`). Push WIP after each finished module.
- **Spec:** `docs/DESIGN.md` is binding (P1 scope = everything not marked P2/P3). `src/proto.rs` is shared by all packages: do not change it. If a change is truly unavoidable, keep it additive (new variant or field), and list it first in your report.
- **Files you own:** only the modules named in your brief, plus unit tests inside them and integration tests under `tests/<your package>_*.rs`. Do not implement other packages' modules (they are `todo!()` stubs being written in parallel). You may change the signatures of your own modules where the contract is wrong; list every change in your report. Cargo.toml: you may add a dependency only if necessary, and list it.
- **Cargo setup in your worktree:** copy `.cargo/config.example.toml` to `.cargo/config.toml` (gitignored), then change `target-dir` to `E:/Development/_cache/cargo-target/crabfarm-<your branch>` and `jobs` to `6`. Three jobs run in parallel on the owner's PC.
- **Checks (proportional, no full-suite or load runs):** `cargo build`, `cargo clippy --all-targets` with no warnings in your modules, `cargo test <your module names>`. Each test must be able to fail: for the key tests, show once that they fail on a deliberately broken implementation (say which in the report). No benchmarks, no CPU or IO load generators.
- **Quality bar:** this is production code that the owner will run on real machines. No `unwrap()` on data from the network or the file system (tests excepted). Errors via `anyhow` with context. No logging of file contents, environment values, keys or tokens. Comments explain why, not what.
- **Rules (one line each):** ask the parent when blocked; record and close only PIDs you started, every launch has a timeout; no persistent environment changes (PATH, registry, setx); compile offline-friendly (use the already-fetched crates; `cargo fetch` is fine if a new dep is needed); coordinate directly with sibling jobs only through pushed branches, no progress notes, final report only.
- **Commits:** identity via environment only: `GIT_AUTHOR_NAME=rennerdo30`, `GIT_AUTHOR_EMAIL=9086097+rennerdo30@users.noreply.github.com`, same for `GIT_COMMITTER_*`. Never write Git config. No AI mentions, `Co-Authored-By` or session lines in commits.

## Brief footer (owner, 2026-10-16)

- **Rules:** read `E:/Development/AGENTS.md`, `E:/Development/general-agent-rules.md` and the repository's `AGENTS.md`/`CLAUDE.md` before work; CLI auto-loading is not a substitute for the brief's explicit rules.
- **Base:** fetch, then start from the exact base ref and commit named in the brief; report your starting base.
- **Scope:** decide everything inside this brief yourself. Ask the parent only for design or owner forks, gate or test changes, destructive or irreversible steps, or widening scope. Batch questions into one message.
- **Identity and Git:** identity via environment only, never write Git config. No AI mentions or session lines in commits. Push only with `git push origin HEAD:refs/heads/<branch>`. Never chain a destructive Git step after one that can fail.
- **Rule files:** never edit `AGENTS.md`, `CLAUDE.md` or shared rules.
- **Processes:** record and close only PIDs you started; every launch has a timeout. No CPU or IO load generators, burners or full-suite local runs on the owner's PC.
- **Multi-round work:** start with a read-only assumption audit and a failing acceptance test; progress is measured by that test.
- **Fix the cause:** never make a misbehaving stage a no-op or drop a feature as a workaround.
- **Facts:** never call an artefact missing without listing every location searched.
- **Hold or stop:** before ending, commit and push work in progress to your ref and say so. Never end with uncommitted work.
- **Native Claude children only:** no bridge, inbox, `message_subagent` or dashboard tools; no hook bypass (`core.hooksPath`); never comment out checks.
- **Report:** what changed (commits), what ran with results, **what did not run**, interface changes, open questions.
