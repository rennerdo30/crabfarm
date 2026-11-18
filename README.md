# crabfarm

Distributed `rustc` over a peer-to-peer farm of your own machines: Windows, macOS and Linux, mixed. Your notebook builds with your desktop's cores.

> Status: in development (P1, Windows to Windows). See [docs/DESIGN.md](docs/DESIGN.md).

## How it will work

```sh
# On the desktop (once)
crabfarm farm create
crabfarm node            # keep running
crabfarm farm invite     # prints a one-time join code

# On the notebook (once)
crabfarm farm join cf1-...@desktop:47070
crabfarm node            # keep running

# Then, instead of `cargo build`:
crabfarm cargo build --release
```

- Every machine runs the same node: it compiles for others and sends its own work out.
- Library crates compile wherever there is free capacity. Linking, build scripts and incremental workspace crates stay local.
- Mutual TLS 1.3 between farm members only. Cached blobs on other machines are encrypted with the submitting machine's key.
- Anything unusual falls back to a local build, so the output is always correct.

## Trust model

Farm members run each other's proc macros and compile each other's code. Only join machines you own.
