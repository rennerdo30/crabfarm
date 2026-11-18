<p align="center"><img src="assets/logo.svg" alt="crabfarm" width="160"></p>

# crabfarm

[![ci](https://github.com/rennerdo30/crabfarm/actions/workflows/ci.yml/badge.svg)](https://github.com/rennerdo30/crabfarm/actions/workflows/ci.yml)

Distributed `rustc` over a peer-to-peer farm of your own machines. Your notebook builds with your desktop's cores. The binaries run on Windows, macOS and Linux; a farm of Windows machines is what 1.0 distributes (see Status).

Website: https://crabfarm.renner.dev

> Status: crabfarm 1.0 distributes builds between Windows machines (same compiler on every machine). Mixed-OS farms are planned. See [docs/DESIGN.md](docs/DESIGN.md).

## Install

New here? The [getting-started guide](docs/GETTING-STARTED.md) walks through installing, creating a farm, joining a second machine and using it in a Rust project.

- **Windows:** run `crabfarm-<version>-setup.exe` from the [Releases page](https://github.com/rennerdo30/crabfarm/releases). It installs per user (no admin rights) and offers to add crabfarm to `PATH` and to start the node when you sign in. The `.zip` is the same without an installer.
- **macOS and Linux:** `curl -fsSL https://github.com/rennerdo30/crabfarm/releases/latest/download/install.sh | sh` (no sudo; checks `SHA256SUMS.txt`; installs into `~/.local/bin`).

- **GitHub Releases:** download the archive for your platform from the [Releases page](https://github.com/rennerdo30/crabfarm/releases) (Windows, Linux and macOS, x86_64 and aarch64), check it against `SHA256SUMS.txt`, unpack it and put `crabfarm`, `crabfarmd` and `cargo-crabfarm` on your `PATH`. The binaries are not code-signed, so Windows SmartScreen or macOS Gatekeeper may warn on the first run. Each archive also has build provenance: `gh attestation verify <archive> --repo rennerdo30/crabfarm`.
- **From crates.io**, once 1.0.0 is published there: `cargo install crabfarm --locked`.

Install it on every machine of the farm. The node also runs without a terminal: `crabfarm service install` starts it at login (Windows Run key, macOS LaunchAgent, Linux systemd user service), `crabfarm node --stop` stops it.

## Quick start

```sh
# On the desktop (once)
crabfarm farm create
crabfarm node            # keep running (own terminal)
crabfarm farm invite     # prints a one-time join code, valid 10 minutes

# On the notebook (once)
crabfarm farm join cf1-...@192.168.1.20:47070
crabfarm node            # keep running (own terminal)

# Then, instead of `cargo build`, in any Rust project on either machine:
crabfarm cargo build --release
cargo crabfarm build --release   # the same thing (cargo runs cargo-crabfarm)
crabfarm status          # peers, free slots, how many jobs ran where

# Or switch a project over once, so plain `cargo build` uses the farm:
crabfarm enable          # writes .cargo/config.toml; `crabfarm disable` undoes it
crabfarm enable --global # every project on this machine
```

- `crabfarm cargo <args>` runs your `cargo` with crabfarm as `RUSTC_WRAPPER` (this replaces a `rustc-wrapper` such as sccache from your cargo config for that run) and, unless you pass `-j`, a job count that covers your slots plus the free slots of the farm. If the node is not running it says so and builds locally.
- `farm invite` uses the first LAN IPv4 address of the machine and the node's port; pass `--addr host:port` for a VPN name or another address.
- Machines find each other through mDNS on the LAN and through the addresses in join codes and gossip. For a VPN or another subnet, list them in `config.toml`: `peers = ["desktop.tailnet:47070"]`. Settings (`slots`, `local_slots`, `prefer_remote`, `mdns`, ...) are in [docs/DESIGN.md](docs/DESIGN.md) section 12; the data directory is `%APPDATA%\renner\crabfarm\data` on Windows, `~/.local/share/crabfarm` on Linux, `~/Library/Application Support/dev.renner.crabfarm` on macOS (`CRABFARM_HOME` overrides it).
- Every machine runs the same node: it compiles for others and sends its own work out.
- Library crates and proc macros compile wherever there is free capacity. Linking, build scripts and incremental workspace crates stay local (use `--release` or a profile without incremental to distribute workspace crates).
- **Compile cache.** Every successful library compile is kept (default 10 GB, `cache = true`), on the machine that submitted it and on the peer that ran it, so a second build, a `cargo clean`, a branch switch or another machine in the farm reuses it instead of compiling again. Same compiler, same arguments, same sources and dependencies: same result, replayed with its warnings. `crabfarm cache stats` shows what it holds, `crabfarm cache clear` empties it, `crabfarm status` counts hits. Paths in `local_only` and `sensitive` are never cached.
- Mutual TLS 1.3 between farm members only. Cached blobs on other machines are encrypted with the submitting machine's key.
- Anything unusual falls back to a local build, so the output is always correct. `crabfarm status` counts every fallback by reason.

### Windows Firewall

The first time `crabfarm node` listens on the network, Windows asks whether to allow it; answering needs administrator rights. Allow `crabfarm` on **private networks** for TCP port 47070 (or the port in your `listen` setting) on the machine that does the work (your desktop). Without it other machines cannot reach this node, and it can only work as a submitter. A notebook that only sends work needs no rule: it connects outward. Set `accept_remote_jobs = false` in its `config.toml` and list the desktop under `peers`; to avoid the prompt entirely also set `listen = "127.0.0.1:47070"` there. The local connection between cargo and the node uses `127.0.0.1` only and needs no rule.

## Trust model

Farm members run each other's proc macros and compile each other's code. Only join machines you own. Details and what is out of scope: [SECURITY.md](SECURITY.md).

## More

- [docs/DESIGN.md](docs/DESIGN.md): how it works
- [CHANGELOG.md](CHANGELOG.md): what changed
- [SECURITY.md](SECURITY.md): reporting a vulnerability, trust model
- [docs/RELEASING.md](docs/RELEASING.md): how releases are made

## License

MIT, see [LICENSE](LICENSE).
