# Getting started

Build Rust projects on your notebook with the cores of your desktop. This guide takes you from nothing to a working farm in about ten minutes. Everything here is per user: no admin rights are needed, except the Windows Firewall rule in step 4, which only the desktop needs.

You need: two or more machines you own, on the same network (or a VPN), and Rust installed on each (`rustup`).

## 1. Install on the desktop

1. Download `crabfarm-<version>-setup.exe` from the [Releases page](https://github.com/rennerdo30/crabfarm/releases) (or use the file you were given).
2. Run it. Windows SmartScreen may say "unknown publisher" because the file is not code-signed: choose **More info**, then **Run anyway**.
3. Leave both boxes ticked:
   - **Add crabfarm to PATH** lets you type `crabfarm` in any new terminal.
   - **Start the crabfarm node when I sign in** keeps the node running in the background (no window).
4. Open a **new** terminal and check:

```powershell
crabfarm --version
```

## 2. Create the farm (desktop, once)

```powershell
crabfarm farm create --name desktop
```

That starts the node in the background. Now make a join code for your other machine:

```powershell
crabfarm farm invite
```

It prints one line starting with `cf1-` (valid 10 minutes, usable once). Copy it. If the desktop has several network addresses and the code points at the wrong one, add `--addr 192.168.1.20:47070` (the desktop's address and port).

## 3. Install on the notebook and join

Run the same setup on the notebook (no admin rights needed, so a managed work notebook is fine as long as you may run programs from your user folder). Then, in a new terminal:

```powershell
crabfarm farm join cf1-PASTE-THE-CODE-HERE --name notebook
```

The node starts by itself. Check that both machines see each other:

```powershell
crabfarm status
```

The first line should say `farm: 1 of 1 peers online` and show the desktop's free slots.

## 4. The Windows Firewall question

The first time the node listens on the network, Windows asks whether to allow `crabfarmd`. Tick **Private networks** only and allow it. Do this on the desktop (it is the machine that works for the others). Answering the prompt needs **administrator rights**: on a managed notebook where you are a standard user you cannot.

If the prompt never appeared, or you are on a Public network profile, an administrator can add the rule once (admin PowerShell):

```powershell
netsh advfirewall firewall add rule name="crabfarm" dir=in action=allow program="%LOCALAPPDATA%\Programs\crabfarm\crabfarmd.exe" protocol=TCP localport=47070 profile=private
```

A notebook that only SENDS work needs no firewall rule: it connects outward to the desktop (every job connection and the control connection are opened by the machine that wants the work done, and the nodes dial each other, so one direction is enough). On such a notebook, put this in its `config.toml` (step 5 shows where) so it never accepts jobs from others:

```toml
accept_remote_jobs = false
```

If you also want to avoid the prompt entirely, let the notebook listen on the loopback address only (`listen = "127.0.0.1:47070"`); nothing then listens on the network. The notebook must still reach the desktop: add it under `peers` (step 5), because discovery on the local network may be blocked by the notebook's firewall too. (Checked in the code: job and control connections to a peer are opened outbound by the submitting node; not tried on a managed Windows notebook.)

## 5. VPN, Tailscale or another network

Machines find each other automatically on the same local network. Across a VPN (or Tailscale), tell each node where the others are. Open `%APPDATA%\renner\crabfarm\data\config.toml` (create it if it does not exist) and add:

```toml
peers = ["desktop.my-tailnet.ts.net:47070"]
```

Use the other machine's VPN name or IP. When you create the join code, give that address: `crabfarm farm invite --addr desktop.my-tailnet.ts.net:47070`.

Settings are read when the node starts. To restart it:

```powershell
crabfarm node --stop
crabfarm service install
```

(`service install` turns autostart on, which is already so, and starts the node because it is not running.)

## 6. Use it in a project

Pick one of three ways. Try A first, then use B.

**A. One command, nothing changed.** Use `crabfarm cargo` instead of `cargo`:

```powershell
crabfarm cargo build --release
```

`cargo crabfarm build --release` does exactly the same (cargo runs the `cargo-crabfarm` program the installer puts next to `crabfarm`).

**B. Per project (recommended).** In the project folder:

```powershell
crabfarm enable
```

This adds two lines to `.cargo/config.toml` of that project (`rustc-wrapper` and `jobs`; three when you already use a wrapper such as `sccache`, which is kept on a line of its own, see below; nothing else is changed, your comments stay). From then on plain `cargo build`, `cargo check` and `cargo test` use the farm. It prints the `jobs` value it chose (your cores plus the free cores in the farm) and how to change it: `crabfarm enable --jobs 40`. Undo with `crabfarm disable`.

Run it anywhere inside the project: in a workspace it writes to the workspace root (cargo ignores a `.cargo/config.toml` inside a member), and it prints which directory it chose. If you already use a wrapper such as `sccache` (in the project, a parent folder or `%USERPROFILE%\.cargo\config.toml`), crabfarm keeps it for the compiles that happen on your own machine and restores your setting on `crabfarm disable`. If the environment variable `RUSTC_WRAPPER` or `CARGO_BUILD_RUSTC_WRAPPER` is set, cargo uses that instead of the config file: `enable` warns, and crabfarm is not used until you unset it.

Commit the file or not: it contains the path of `crabfarm.exe` on your machine, so most people add `.cargo/config.toml` to `.gitignore` instead of sharing it.

**C. Everywhere.** `crabfarm enable --global` does the same in `%USERPROFILE%\.cargo\config.toml`, so every Rust project on this machine uses the farm. Undo with `crabfarm disable --global`. If the node is not running, builds just run locally; nothing breaks.

**IDE and rust-analyzer.** With B or C, rust-analyzer's background `cargo check` also runs with crabfarm as the wrapper, so library crates it checks can be compiled by the farm too. If your editor behaves oddly, run `crabfarm disable` in the project and tell us. Tools that set the `RUSTC_WRAPPER` environment variable themselves (some CI scripts) override the config file; then crabfarm is simply not used.

## 7. Work repositories

crabfarm sends your project's source code to your own machines to compile it there. The traffic is encrypted and only machines in your farm can connect, but be clear about what that means:

- A peer sees the plaintext source it compiles, and the code of proc macros runs on it, so every machine in the farm must be one you control. Do not join a machine you do not trust.
- If your employer restricts where code may be copied, check that a second company machine is allowed. A private desktop at home is not.

Two settings in `config.toml` (same file as above, on the machine you build from) limit what leaves the machine:

```toml
# Never sent to a peer: these projects always compile locally.
local_only = ['C:\Work\secret-project', 'C:\Work\customer-x']

# Sent, but peers delete it right after each job instead of keeping a cache.
sensitive = ['C:\Work']
```

- `local_only` is the strong one: crates whose source or manifest lies under one of those folders are never sent. Their dependencies from crates.io may still be, because they are public; dependencies you keep under the folder are covered.
- `sensitive` is weaker: the peer still reads the source while compiling; it just does not keep it afterwards. Nothing under `sensitive` or `local_only` enters the compile cache (step 8) either.
- For a work notebook the safe setup is: `local_only = ['C:\Work']` on the notebook. It can still serve builds for the desktop, which is the useful direction. Then the notebook stays a plain helper and none of the work code travels.

Restart the node after changing the file (`crabfarm node --stop`, then `crabfarm service install`).

## 8. Check that it works

Build something, then:

```powershell
crabfarm status
```

```
farm: 1 of 1 peers online, 28 free remote slots
...
jobs: 41 remote ok, 0 remote failed, 12 local, 0 running remote
local because: NoPeer 2, ...
```

- `remote ok`: compiles a peer did for you. This number should grow when you build.
- `local`: compiles that ran on your own machine, with the reason beside it. Linking, build scripts and the final binary always run locally; that is expected.
- `remote failed`: a peer could not do a job and your machine redid it. A few are normal; many mean something is wrong (see below).
- Slots are how many compiles a machine runs at once.

### The compile cache

Every library crate crabfarm compiles, here or on a peer, is also kept in a cache (default limit 10 GB). The next time the same crate comes up with the same compiler, flags, source files and dependencies, the stored result is used and rustc does not run: after `cargo clean`, after switching branches back and forth, and on another machine in the farm that builds the same project at the same path. A crate that uses a proc macro is kept from a local run only after it has built successfully on a peer (proc macros can read any file while rustc runs, and only a peer's sandbox shows that they do not need one). `crabfarm status` shows a line like `compile cache: 41 hits (35 local, 6 farm), 3 misses, ...`.

```powershell
crabfarm cache stats    # entries and size on disk, hit counters
crabfarm cache clear    # delete everything the cache holds on this machine
```

In `config.toml`: `cache = false` turns it off on this machine, `cache_entries_max_gb = 10` sets its size. Farm hits need the other machine to have the project at the same path (compiler messages and files name absolute paths); entries from a machine whose path differs are simply not found.

## 9. If something is wrong

- **`the node is not running`**: run `crabfarm service status`. If autostart is off: `crabfarm service install`. Or start it by hand in a terminal with `crabfarm node` and read what it says.
- **Everything runs locally (`NoPeer`)**: the machines do not see each other. Check the firewall (step 4), that both are on the same network, or add `peers` (step 5).
- **First build is slow**: the first time, the desktop may have to install the Rust version your project needs (via `rustup`). That can take a few minutes once.
- **Version mismatch**: an error like `peer speaks protocol 5, this node speaks 6` means two machines run different crabfarm versions. Install the same version on both.
- **The log**: `%APPDATA%\renner\crabfarm\data\logs\node.log`. If the node does not start at all, the reason is the last lines of that file. It holds crate names, sizes and timings, never your code.
- **Stop the node**: `crabfarm node --stop`.

## 10. Uninstall

Settings, then Apps, then crabfarm, then Uninstall (or run `uninstall.exe` in `%LOCALAPPDATA%\Programs\crabfarm`). It stops the node, turns autostart off and removes crabfarm from PATH. Your farm identity and cache stay unless you tick **Also delete my farm identity and cache** on the uninstall page; keeping them lets a reinstall rejoin the same farm.

The uninstaller removes a global entry (`crabfarm enable --global`) that points at this install. Entries made with `crabfarm enable` **inside projects** are not found by it: run `crabfarm disable` in each of those projects BEFORE uninstalling (or install crabfarm again and run it then), otherwise cargo complains that the wrapper `crabfarm.exe` is gone.

---

## macOS and Linux

Install from a terminal. It never uses sudo and puts the programs in `~/.local/bin`:

```sh
curl -fsSL https://github.com/rennerdo30/crabfarm/releases/latest/download/install.sh | sh
```

(or download `install.sh` and the archive, then `sh install.sh ./crabfarm-<version>-<target>.tar.gz`). `cargo crabfarm` works too (`cargo-crabfarm` is installed next to `crabfarm`). Make sure `~/.local/bin` is on your `PATH`: `crabfarm path add` prints the line to add to your shell profile. Then follow the same steps as above; `crabfarm service install` makes the node start at login (a LaunchAgent on macOS, a systemd user service on Linux). Paths differ: the config is `~/.local/share/crabfarm/config.toml` on Linux and `~/Library/Application Support/dev.renner.crabfarm/config.toml` on macOS.
