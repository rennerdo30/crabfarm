# CF-3 Package B: farm identity, mutual TLS transport, discovery

Read `docs/briefs/common.md` first; it is part of this brief. Pair Desk issue: `CF-3`. Branch: `cf-3-transport`.

**Goal:** implement `src/farm.rs`, `src/transport.rs`, `src/discovery.rs` per DESIGN sections 3, 4 and 11 and the stub doc comments.

**Security requirements (each needs a test that fails when the check is removed)**
- TLS 1.3 only (`rustls` 0.23 with the ring provider, `with_protocol_versions(&[&TLS13])`). No TLS 1.2, no anonymous mode.
- Server side: client certificates are optional at the TLS layer (needed so a joining node without a cert can connect), but `accept` requires a farm-CA-signed client cert for every connection whose first frame is not a `JoinRequest`. Use `WebPkiClientVerifier` with the farm CA as the only root and `allow_unauthenticated()`.
- Client side: a custom `ServerCertVerifier` that verifies the server's end-entity cert against the farm CA (rustls-webpki, server-auth usage, validity time) and ignores the hostname. Signature verification delegates to the ring provider's algorithms.
- Node certs: Ed25519 (rcgen 0.14), CN = node name, SAN URI `crabfarm:node:<node id hex>`, both server-auth and client-auth EKUs, 10-year validity, CA = Ed25519, `is_ca` with path length 0. `node_id_from_cert` reads the SAN; `connect`/`accept` reject a Hello whose node id differs from the cert's.
- Join: `JoinCode` text format exactly as DESIGN section 3 (base32 RFC 4648 lowercase without padding via `data-encoding`). The joiner pins the inviter's certificate by the first 16 bytes of SHA-256 of its DER (custom verifier). Tokens: 16 random bytes, single use, 10-minute expiry, constant-time compare (`subtle`), persisted in `farm/invites.toml` so the `crabfarm farm invite` CLI process can create them while a node process redeems them. Only a node with `ca.key` can sign.
- Key files written owner-only: on Unix mode 0600; on Windows, create files in the per-user data dir (inherits the user's profile ACL); note in a comment that explicit ACL hardening is P2.

**Interface change you should make:** `accept` must handle the join path. Change it to return an enum, e.g. `Accepted::Peer { node_id, hello, conn }` / `Accepted::Join { request: Message, conn }`, and add `farm::handle_join(id: &Identity, invites: &mut Invites, data_dir: &Path, request: &Message) -> anyhow::Result<Message>` returning `JoinAccepted` or an `Error`. List these in your report. Package D wires them into the node.

**Discovery:** `PeerBook` (toml) with `merge` per stub doc; `Mdns` using `mdns-sd` 0.22: service `_crabfarm._tcp.local.`, instance = node id hex, TXT `f=<short farm id>`, `v=<PROTO_VERSION>`; browsing filters on `f`. A unit test for `merge`; mDNS gets a smoke test that is `#[ignore]` by default (multicast may be blocked) and you run it once by hand with the result in your report.

**Tests (loopback, in-process, tempdirs, `127.0.0.1:0` ports)**
1. Farm create → invite → join from a second data dir → both identities load → mTLS `connect`/`accept` succeed and exchange a few frames including one at 15 MiB.
2. Rejections: a node from a *different* farm (own CA) can neither connect to nor accept from the farm; a client without a cert gets refused for a non-join first frame; wrong, reused and expired tokens are refused; wrong pin fingerprint fails the join; oversized frame (> MAX_FRAME) is refused without allocating it.
3. `JoinCode` encode/parse round trip and malformed codes.

**Acceptance (finite list)**
1. Three modules implemented, no `todo!()` left in them.
2. All tests above green; each security test shown failing once against a deliberately weakened check (say which).
3. `cargo clippy --all-targets` clean for these modules.
4. Branch pushed; final report per common brief.
