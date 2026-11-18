# crabfarm

Distributed `rustc` over a peer-to-peer farm of your own machines: Windows, macOS and Linux, mixed. Your notebook builds with your desktop's cores.

> Status: P1 (Windows to Windows, same compiler on every machine). See [docs/DESIGN.md](docs/DESIGN.md).

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
crabfarm status          # peers, free slots, how many jobs ran where
```

- `crabfarm cargo <args>` runs your `cargo` with crabfarm as `RUSTC_WRAPPER` (this replaces a `rustc-wrapper` such as sccache from your cargo config for that run) and, unless you pass `-j`, a job count that covers your slots plus the free slots of the farm. If the node is not running it says so and builds locally.
- `farm invite` uses the first LAN IPv4 address of the machine and the node's port; pass `--addr host:port` for a VPN name or another address.
- Machines find each other through mDNS on the LAN and through the addresses in join codes and gossip. For a VPN or another subnet, list them in `config.toml`: `peers = ["desktop.tailnet:47070"]`. Settings (`slots`, `local_slots`, `prefer_remote`, `mdns`, ...) are in [docs/DESIGN.md](docs/DESIGN.md) section 12; the data directory is `%APPDATA%\renner\crabfarm\data` on Windows, `~/.local/share/crabfarm` on Linux, `~/Library/Application Support/dev.renner.crabfarm` on macOS (`CRABFARM_HOME` overrides it).
- Every machine runs the same node: it compiles for others and sends its own work out.
- Library crates and proc macros compile wherever there is free capacity. Linking, build scripts and incremental workspace crates stay local (use `--release` or a profile without incremental to distribute workspace crates).
- Mutual TLS 1.3 between farm members only. Cached blobs on other machines are encrypted with the submitting machine's key.
- Anything unusual falls back to a local build, so the output is always correct. `crabfarm status` counts every fallback by reason.

### Windows Firewall

The first time `crabfarm node` listens on the network, Windows asks whether to allow it. Allow `crabfarm` on **private networks** for TCP port 47070 (or the port in your `listen` setting). Without it other machines cannot reach this node, and it can only work as a submitter. The local connection between cargo and the node uses `127.0.0.1` only and needs no rule.

## Trust model

Farm members run each other's proc macros and compile each other's code. Only join machines you own.
