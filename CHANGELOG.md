# Changelog

## Unreleased

First working version (P1: Windows to Windows).

- **One binary:** `crabfarm node`, `farm create|invite|join`, `cargo <args>`, `status`, and the rustc wrapper mode that cargo calls.
- **Farm identity:** a farm CA created on the first machine; machines join with a one-time join code (valid 10 minutes, single use) and get their own certificate.
- **Transport:** mutually authenticated TLS 1.3 between farm members only; no plaintext or anonymous mode. The local wrapper talks to the node over loopback with a per-start random token.
- **Distribution:** library crates compile on whichever machine has free capacity; linking, build scripts and anything unusual stay local, so the output matches a local build.
- **Cached inputs:** blobs kept on other machines are encrypted with the submitting machine's key; job sandboxes are deleted when the job ends.
- **Fails safe:** a missing peer, missing toolchain, network error or unsupported flag runs the job locally; fallbacks are counted in `crabfarm status`.
