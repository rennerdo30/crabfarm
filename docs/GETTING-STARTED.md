# Getting started

Build Rust projects on your notebook with the cores of your desktop. This guide takes you from nothing to a working farm in about ten minutes. Everything here is per user: no admin rights are needed, except the optional firewall command in step 4.

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

The first time the node listens on the network, Windows asks whether to allow `crabfarmd`. Tick **Private networks** only and allow it. Do this on the desktop at least (it is the machine that works for the others).

If the prompt never appeared, or you are on a Public network profile, an administrator can add the rule once (admin PowerShell):

```powershell
netsh advfirewall firewall add rule name="crabfarm" dir=in action=allow program="%LOCALAPPDATA%\Programs\crabfarm\crabfarmd.exe" protocol=TCP localport=47070 profile=private
```

The notebook only needs outgoing connections if it just sends work to the desktop, but allowing the rule on both is simplest.

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

**B. Per project (recommended).** In the project folder:

```powershell
crabfarm enable
```

This adds two lines to `.cargo/config.toml` of that project (`rustc-wrapper` and `jobs`, nothing else is changed, your comments stay). From then on plain `cargo build`, `cargo check` and `cargo test` use the farm. It prints the `jobs` value it chose (your cores plus the free cores in the farm) and how to change it: `crabfarm enable --jobs 40`. Undo with `crabfarm disable`.

Run it in the folder that holds the project's `Cargo.toml`; for a workspace, run it in the workspace root. If you already use a wrapper such as `sccache`, crabfarm keeps it for the compiles that happen on your own machine and restores your setting on `crabfarm disable`.

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
- `sensitive` is weaker: the peer still reads the source while compiling; it just does not keep it afterwards.
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

## 9. If something is wrong

- **`the node is not running`**: run `crabfarm service status`. If autostart is off: `crabfarm service install`. Or start it by hand in a terminal with `crabfarm node` and read what it says.
- **Everything runs locally (`NoPeer`)**: the machines do not see each other. Check the firewall (step 4), that both are on the same network, or add `peers` (step 5).
- **First build is slow**: the first time, the desktop may have to install the Rust version your project needs (via `rustup`). That can take a few minutes once.
- **Version mismatch**: an error like `peer speaks protocol 3, this node speaks 4` means two machines run different crabfarm versions. Install the same version on both.
- **The log**: `%APPDATA%\renner\crabfarm\data\logs\node.log`. If the node does not start at all, the reason is the last lines of that file. It holds crate names, sizes and timings, never your code.
- **Stop the node**: `crabfarm node --stop`.

## 10. Uninstall

Settings, then Apps, then crabfarm, then Uninstall (or run `uninstall.exe` in `%LOCALAPPDATA%\Programs\crabfarm`). It stops the node, turns autostart off and removes crabfarm from PATH. Your farm identity and cache stay unless you tick **Also delete my farm identity and cache** on the uninstall page; keeping them lets a reinstall rejoin the same farm.

Run `crabfarm disable` (or `crabfarm disable --global`) in projects where you used `crabfarm enable` first, otherwise cargo will complain that the wrapper `crabfarm.exe` is gone.

---

## macOS and Linux

Install from a terminal. It never uses sudo and puts the programs in `~/.local/bin`:

```sh
curl -fsSL https://github.com/rennerdo30/crabfarm/releases/latest/download/install.sh | sh
```

(or download `install.sh` and the archive, then `sh install.sh ./crabfarm-<version>-<target>.tar.gz`). Make sure `~/.local/bin` is on your `PATH`: `crabfarm path add` prints the line to add to your shell profile. Then follow the same steps as above; `crabfarm service install` makes the node start at login (a LaunchAgent on macOS, a systemd user service on Linux). Paths differ: the config is `~/.local/share/crabfarm/config.toml` on Linux and `~/Library/Application Support/dev.renner.crabfarm/config.toml` on macOS.
