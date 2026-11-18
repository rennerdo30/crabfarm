# Changelog

## Unreleased

First working version (P1: Windows to Windows).

- **One binary:** `crabfarm node`, `farm create|invite|join`, `cargo <args>`, `status`, and the rustc wrapper mode that cargo calls.
- **Farm identity:** a farm CA created on the first machine; machines join with a one-time join code (valid 10 minutes, single use) and get their own certificate.
- **Transport:** mutually authenticated TLS 1.3 between farm members only; no plaintext or anonymous mode. The local wrapper talks to the node over loopback with a per-start random token.
- **Distribution:** library crates compile on whichever machine has free capacity; linking, build scripts and anything unusual stay local, so the output matches a local build.
- **Cached inputs:** blobs kept on other machines are encrypted with the submitting machine's key; job sandboxes are deleted when the job ends.
- **Cargo pipelining:** metadata and dep-info results are relayed as soon as they exist, so cargo can start dependent crates before the full build finishes.
- **Hardening:** peer-supplied paths, toolchain specs and job specs are validated; the executor uses an environment allowlist and size caps; a job's whole process tree is killed on timeout; outputs that embed the sandbox path are rerun locally; connection, blob and discovery limits and idle timeouts guard against misbehaving peers; join codes, keys and environment values never appear in logs.
- **Publication:** MIT license, release archives for Windows, Linux and macOS (x86_64 and aarch64) with checksums and build provenance, a website at https://crabfarm.renner.dev.
- **Fails safe:** a missing peer, missing toolchain, network error or unsupported flag runs the job locally; fallbacks are counted in `crabfarm status`.
