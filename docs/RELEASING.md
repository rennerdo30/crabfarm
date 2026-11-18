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

The first release may use the version already in `Cargo.toml` (0.1.0).

## What CI does ([`release.yml`](../.github/workflows/release.yml))

On the tag push:
- It checks that the tag matches the `Cargo.toml` version.
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
- It writes one `SHA256SUMS.txt` over all archives and creates a **draft** release with the changelog section as notes.
- Rerunning it (Actions, release, Run workflow, with the tag) refreshes a draft and never touches a published release.

`macos-15-intel` is the last Intel Mac runner label GitHub offers on the free tier. When it is retired, build `x86_64-apple-darwin` on `macos-latest` after `rustup target add x86_64-apple-darwin`.

The owner reviews the draft and publishes it.

## crates.io

[`publish-crate.yml`](../.github/workflows/publish-crate.yml) is manual (Actions, publish-crate, Run workflow, with the tag). Run it after the GitHub release is published.

One-time setup by the owner (the agents never create secrets):
1. Create a crates.io API token scoped to publishing the `crabfarm` crate (crates.io, Account Settings, API Tokens).
2. In the repository: Settings, Environments, New environment `crates-io`; add **Required reviewers** (the owner) so each upload needs an approval.
3. Add the token as the environment secret `CARGO_REGISTRY_TOKEN`.

A crates.io version can never be overwritten, only yanked. `cargo package --list` shows what ships (`include` in `Cargo.toml`: `src`, `Cargo.toml`, `Cargo.lock`, `LICENSE`, `README.md`, `CHANGELOG.md`, `docs/DESIGN.md`).

## Website

`website/` deploys to GitHub Pages as https://crabfarm.renner.dev ([`pages.yml`](../.github/workflows/pages.yml)) when it changes on `main`, or by hand. One-time setup by the owner:
1. Settings, Pages, Source: **GitHub Actions**; custom domain `crabfarm.renner.dev` (the `CNAME` file in `website/` carries it); enforce HTTPS once the certificate is issued.
2. DNS: `CNAME crabfarm -> rennerdo30.github.io`.

## What protects a download

- **The checksum protects integrity, not authenticity.** It comes from the same release as the archive.
- **The binaries are not code-signed.** Windows SmartScreen and macOS Gatekeeper may warn on the first run of a downloaded copy; `cargo install crabfarm` builds on your machine and has no such prompt. Signing is a separate owner decision.
