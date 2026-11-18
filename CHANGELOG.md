# Changelog

## Unreleased

First release, 1.0.0 (Windows to Windows farms).

- **Binaries:** `crabfarm` (`node`, `farm create|invite|join`, `cargo <args>`, `status`, and the rustc wrapper mode that cargo calls), `crabfarmd` (the node without a console window) and `cargo-crabfarm` (`cargo crabfarm build` = `crabfarm cargo build`).
- **Farm identity:** a farm CA created on the first machine; machines join with a one-time join code (valid 10 minutes, single use) and get their own certificate.
- **Transport:** mutually authenticated TLS 1.3 between farm members only; no plaintext or anonymous mode. The local wrapper talks to the node over loopback with a per-start random token.
- **Distribution:** library crates compile on whichever machine has free capacity; linking, build scripts and anything unusual stay local, so the output matches a local build.
- **Cached inputs:** blobs kept on other machines are encrypted with the submitting machine's key; job sandboxes are deleted when the job ends.
- **Cargo pipelining:** metadata and dep-info results are relayed as soon as they exist, so cargo can start dependent crates before the full build finishes.
- **Hardening:** peer-supplied paths, toolchain specs and job specs are validated; the executor uses an environment allowlist and size caps; a job's whole process tree is killed on timeout; outputs that embed the sandbox path are rerun locally; connection, blob and discovery limits and idle timeouts guard against misbehaving peers; join codes, keys and environment values never appear in logs.
- **Install:** a per-user Windows setup (`crabfarm-<version>-setup.exe`: optional PATH entry and start at sign-in, no admin rights) and `install.sh` for macOS and Linux (checksum-verified, no sudo); `crabfarmd`, the node without a console window; `crabfarm service install|uninstall|status`, `crabfarm path add|remove`, `crabfarm node --stop`; `crabfarm enable|disable [--global]` so plain `cargo build` uses the farm (`enable` writes to the workspace root and says which folder it chose; an existing wrapper such as sccache, also one from a parent folder or `CARGO_HOME`, is kept for local compiles and restored by `disable`; a `RUSTC_WRAPPER` in the environment is warned about); a step-by-step guide (`docs/GETTING-STARTED.md`, https://crabfarm.renner.dev/guide.html).
- **Publication:** MIT license, release archives for Windows, Linux and macOS (x86_64 and aarch64) with checksums and build provenance, a website at https://crabfarm.renner.dev.
- **Fails safe:** a missing peer, missing toolchain, network error or unsupported flag runs the job locally; fallbacks are counted in `crabfarm status`.
- **Setup and uninstall (CF-27):** a running node is stopped and the result checked; programs that cargo or rust-analyzer still run are renamed aside instead of failing the update; an uninstall that cannot remove files keeps its Installed apps entry, says so and exits non-zero (silent mode); the uninstaller removes a global `crabfarm enable --global` entry that points at this install.
- **Environment never-list (CF-27):** every `GIT_` name except a short list of build-script data names, plus C runtime, locale, allocator, Visual Studio/SDK discovery and OpenSSL configuration variables.
- **Faster fallback (CF-27):** opening a job on a peer has one 12 s deadline; a peer's addresses are tried at the same time.
