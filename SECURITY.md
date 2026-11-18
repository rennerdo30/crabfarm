# Security policy

## Reporting a vulnerability

Email **support@renner.dev** with a description, the affected version (`crabfarm --version`) and, if you can, steps to reproduce. Please do **not** open a public issue or pull request for a vulnerability. You will get an answer, and a fix or a decision, as soon as practical; please keep the details private until a fixed release is out.

Do not send join codes, keys or `ca.key` files, even from a test farm you care about.

## Supported versions

Only the latest release receives security fixes. Update to the newest release (GitHub Releases, or `cargo install crabfarm --force`) before reporting, if you can.

## Trust model in plain words

- **Farm members fully trust each other.** A machine in your farm compiles other members' code and runs their proc macros and build-time code, so any member can run arbitrary code on any other member. Join only machines you own and control. crabfarm is not a sandbox against a hostile farm member.
- **Environment names are part of that trust.** A job carries only cargo's own variables plus names the submitter listed (`forward_env`, which a node accepts only if its operator listed them in `accept_env`) and the names a crate's build script declared with `cargo:rustc-env` (`build_env`). By default (`accept_build_env = true`) a node accepts the declared build-script names, because the submitter's build script already runs arbitrary code on the submitter and members trust each other fully; set `accept_build_env = false` to make every such job run on its submitter. Independent of both settings a never-list is refused: `PATH`, loader and wrapper variables (`LD_*`, `DYLD_*`, `RUSTC*`, `RUSTUP*`), machine variables, tool search paths (`LIB`, `LIBPATH`, `INCLUDE`, `COMPILER_PATH`, `GCC_EXEC_PREFIX`, ...), linker and cl options (`LINK`, `_LINK_`, `CL`, `_CL_`), proxies (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY` in any case), `SSL_*`, every `GIT_` name except the build-script data names `GIT_HASH`, `GIT_SHA`, `GIT_SHORT_HASH`, `GIT_COMMIT`, `GIT_COMMIT_HASH`, `GIT_COMMIT_DATE`, `GIT_BRANCH`, `GIT_DESCRIBE`, `GIT_TAG`, `GIT_VERSION` (exact names; new git variables are blocked by default), the C runtime and tool-discovery names `GLIBC_TUNABLES`, `GCONV_PATH`, `LOCPATH`, `NLSPATH`, `MALLOC_*`, `VCINSTALLDIR`, `VSINSTALLDIR`, `VCToolsInstallDir`, `VSCMD_*`, `WindowsSdkDir`, `WindowsSDKVersion`, `UniversalCRTSdkDir`, `__COMPAT_LAYER`, `RUST_TARGET_PATH`, `DEPENDENCIES_OUTPUT`, `SUNPRO_DEPENDENCIES`, `OPENSSL_CONF`, `OPENSSL_MODULES`, `SSH_*`, `PYTHON*`, `PERL5*`, `RUBY*`, `NODE_OPTIONS` and `JAVA_TOOL_OPTIONS`. A declared name never overrides a variable of the peer's own environment.
- **Only farm members can connect.** Every connection between machines is mutual TLS 1.3 against the farm's own certificate authority. There is no plaintext or anonymous mode. A join code is single use and valid for 10 minutes; treat it like a password while it is valid.
- **At rest on peers.** Source blobs that a peer keeps for later jobs are encrypted with the submitting machine's key, which other machines never store. Plaintext exists only in the job's sandbox while `rustc` runs, and the sandbox is deleted afterwards. Disk encryption (BitLocker, FileVault, LUKS) on every farm machine is still recommended.
- **Compile cache.** crabfarm keeps the results of rustc runs (output files and printed lines) so that another build, on this machine or on another farm member, can reuse them. Entries are encrypted with a key derived from a secret that every farm member holds (made when the farm was created and handed out when a machine joins), so any member can read what another member's job produced and nothing outside the farm can. That is the same trust as above: a member sees the code it compiles anyway, and a member that serves a wrong result could poison a build. Nothing from a path you listed under `sensitive` or `local_only` is ever cached, by any machine, and a node with `retain_blobs = false` caches nothing. A cached file is used only if it authenticates and has the hash the entry names; otherwise the build simply runs. A result of a local run is kept when no proc macro takes part in the compile (rustc then reads only what the key and the dep-info cover), and otherwise only while that crate with those exact proc-macro builds has built successfully on a peer (whose sandbox held only the shipped files) and has not failed there since; a proc macro that needs a file outside the package directory therefore never gets a stale cached result. A macro that reads such a file only when it exists cannot be detected. Questions about entries travel on the authenticated control connections, never more than two at a time per peer. `cache = false` turns the cache off on a machine, `crabfarm cache clear` empties it.
- **Local side.** The node's control port listens on `127.0.0.1` only and requires a random per-start token readable only by your user.

### Out of scope

- A farm member that is malicious or compromised (see above), including its proc macros and build scripts.
- An attacker who already has your user account or the files in the crabfarm data directory on a machine (keys, `ca.key`).
- Attacks that need the farm CA key.
- Denial of service by a farm member beyond the documented connection and size limits.
- Weaknesses in `rustc`, `cargo` or third-party crates that crabfarm only calls (report those upstream; tell us if crabfarm makes them worse).
- Release binaries are not code-signed (see `docs/RELEASING.md`); unsigned-binary warnings are not vulnerabilities. Downloads carry a SHA256 checksum and build provenance.
