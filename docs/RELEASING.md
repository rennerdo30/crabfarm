# Releasing crabfarm

Every release needs the owner's yes. The script prepares and tags the commit, CI builds the packages and prepares a **draft** GitHub release, and publishing that draft is the owner's yes. The crates.io upload is a separate manual step. Nothing publishes on its own.

## Versions

One version, `version` under `[package]` in `Cargo.toml`, following SemVer.
- Prereleases look like `0.2.0-beta.1`. Their tags have a `-` and become GitHub prereleases.
- A stable `0.2.0` ranks above every `0.2.0-...`.

## Cutting a release

From a clean checkout of `main` that matches `origin/main`:

```powershell
./scripts/release.ps1 -Version 0.2.0 -DryRun   # reports what would stop it; changes nothing
./scripts/release.ps1 -Version 0.2.0
```

Set the public commit identity in the environment first (`GIT_AUTHOR_*` and `GIT_COMMITTER_*`; the script never writes Git config and refuses anything but a GitHub noreply address).

The script:
1. refuses a dirty tree, a branch other than `main`, a `main` that differs from `origin/main`, an existing tag, a version that does not go up, and an empty `## Unreleased` section in `CHANGELOG.md`;
2. sets the version in `Cargo.toml` and `Cargo.lock`;
3. moves the Unreleased notes into a `## X.Y.Z` section;
4. runs `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings` and `cargo test --locked` (`-SkipTests` skips the last; CI runs it again), and restores the edited files if a check fails;
5. commits `Release X.Y.Z` and creates the annotated tag `vX.Y.Z`;
6. pushes with explicit refs: `git push origin HEAD:refs/heads/main`, then `git push origin refs/tags/vX.Y.Z`.

The first release may use the version already in `Cargo.toml` (0.1.0), or a prerelease of it (`0.1.0-rc.1`, see the dry run below).

## Dry run before the first real release

Run the whole pipeline once with a release candidate tag, so the first public release is not also the first run of `release.yml`:

1. Add a line to `## Unreleased` in `CHANGELOG.md` if it is empty, then `./scripts/release.ps1 -Version 0.1.0-rc.1`. The tag has a `-`, so CI builds all six packages and creates a **draft prerelease** only; nothing is published.
2. Check the draft: six archives, `SHA256SUMS.txt`, the notes, and `gh attestation verify <archive> --repo rennerdo30/crabfarm`. Unpack one and run `crabfarm --version`.
3. Delete the draft (GitHub, Releases) and the tag (`git push origin :refs/tags/v0.1.0-rc.1`, then `git tag -d v0.1.0-rc.1`; the owner's yes, since it removes a remote ref). The `Release 0.1.0-rc.1` commit stays on `main`.
4. Put the fixes the candidate showed into `## Unreleased` and cut `0.1.0` as usual.

Never run the crates.io workflow for a candidate.

## What CI does ([`release.yml`](../.github/workflows/release.yml))

On the tag push:
- It checks that the tagged commit is on `main` (`git merge-base --is-ancestor`) and that the tag matches the `Cargo.toml` version.
- It builds, tests and packages, each natively on a GitHub-hosted runner:

  | Target | Runner | Archive |
  |---|---|---|
  | `x86_64-pc-windows-msvc` | `windows-latest` | `.zip` |
  | `aarch64-pc-windows-msvc` | `windows-11-arm` | `.zip` |
  | `x86_64-unknown-linux-gnu` | `ubuntu-24.04` | `.tar.gz` |
  | `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | `.tar.gz` |
  | `x86_64-apple-darwin` | `macos-15-intel` | `.tar.gz` |
  | `aarch64-apple-darwin` | `macos-latest` | `.tar.gz` |

  Each archive is named `crabfarm-<version>-<target>` and holds the binary, `LICENSE` and `README.md`.
- It attaches signed build provenance to every archive (GitHub artifact attestations, `actions/attest-build-provenance`; the `id-token` and `attestations` permissions exist only in that job). Verify a download with `gh attestation verify <archive> --repo rennerdo30/crabfarm`.
- It writes one `SHA256SUMS.txt` over all archives and creates a **draft** release with the changelog section as notes.
- Rerunning it (Actions, release, Run workflow, with the tag) refreshes a draft and never touches a published release.

`macos-15-intel` is the last Intel Mac runner label GitHub offers on the free tier. When it is retired, build `x86_64-apple-darwin` on `macos-latest` after `rustup target add x86_64-apple-darwin`.

Runner labels were checked on 2026-11-19 against GitHub's "GitHub-hosted runners" reference (docs.github.com, Standard runners, free on public repositories). Recheck them before each release; the check date is also in a comment in `release.yml`.

The owner reviews the draft and publishes it.

## crates.io

[`publish-crate.yml`](../.github/workflows/publish-crate.yml) is manual (Actions, publish-crate, Run workflow, with the tag). Run it after the GitHub release is published.

One-time setup by the owner (the agents never create secrets):
1. Create a crates.io API token scoped to publishing the `crabfarm` crate (crates.io, Account Settings, API Tokens).
2. In the repository: Settings, Environments, New environment `crates-io`; add **Required reviewers** (the owner) so each upload needs an approval.
3. Add the token as an **environment secret** named `CARGO_REGISTRY_TOKEN` in `crates-io` (not a repository secret; a repository secret would be readable by every workflow).

The workflow checks out `refs/tags/<tag>`, runs `cargo package --locked` (which builds the packaged crate to verify it) in a step **without** the token, and only then runs `cargo publish --locked --no-verify` with the token in that one step's environment. No compiled code (build scripts, proc macros) ever runs next to the token.

**Switching to Trusted Publishing after the first release.** crates.io can issue a short-lived token over OIDC so no long-lived secret is stored, but the crate must already exist: the first release is published with the token above, then (crates.io, the crate's Settings, Trusted Publishing) add the GitHub repository `rennerdo30/crabfarm`, workflow `publish-crate.yml`, environment `crates-io`. Then change the workflow: give the job `permissions: id-token: write`, add a step `rust-lang/crates-io-auth-action` (pinned to a commit SHA) with `id: auth`, pass `${{ steps.auth.outputs.token }}` as `CARGO_REGISTRY_TOKEN` to the publish step, and delete the stored secret and the API token. The package-and-verify step stays token-free. Source: the Rust blog post "crates.io: development update" of 2025-07-11 and https://crates.io/docs/trusted-publishing.

A crates.io version can never be overwritten, only yanked. `cargo package --list` shows what ships (`include` in `Cargo.toml`: `src`, `Cargo.toml`, `Cargo.lock`, `LICENSE`, `README.md`, `CHANGELOG.md`, `docs/DESIGN.md`).

## Website

`website/` deploys to GitHub Pages as https://crabfarm.renner.dev ([`pages.yml`](../.github/workflows/pages.yml)) when it changes on `main`, or by hand. One-time setup by the owner:
1. Settings, Pages, Source: **GitHub Actions**; custom domain `crabfarm.renner.dev` (the `CNAME` file in `website/` carries it); enforce HTTPS once the certificate is issued.
2. DNS: `CNAME crabfarm -> rennerdo30.github.io`.

## What protects a download

- **The checksum protects integrity, not authenticity.** It comes from the same release as the archive.
- **The binaries are not code-signed.** Windows SmartScreen and macOS Gatekeeper may warn on the first run of a downloaded copy; `cargo install crabfarm` builds on your machine and has no such prompt. Signing is a separate owner decision.
