#!/bin/sh
# crabfarm installer for macOS and Linux. Never uses sudo; installs into
# ~/.local/bin (override: CRABFARM_BIN_DIR). Edits no shell profile, no PATH.
#
#   sh install.sh                      latest release from GitHub
#   sh install.sh ./crabfarm-0.1.0-x86_64-unknown-linux-gnu.tar.gz
#                                      a local archive (SHA256SUMS.txt next to it is checked)
#
# Environment (all optional):
#   CRABFARM_VERSION        release to download, for example 0.1.0 (default: latest)
#   CRABFARM_BIN_DIR        where to install (default: $HOME/.local/bin)
#   CRABFARM_TARGET         override the detected target triple
#   CRABFARM_SKIP_VERIFY=1  install a local archive without a SHA256SUMS.txt
#   CRABFARM_BASE_URL       download base (tests); default is the GitHub release
set -eu

REPO="rennerdo30/crabfarm"

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -ne 0 ] || die "do not run this as root or with sudo; it installs for your user only"
[ -n "${HOME:-}" ] || die "HOME is not set"

target="${CRABFARM_TARGET:-}"
if [ -z "$target" ]; then
  os=$(uname -s)
  arch=$(uname -m)
  case "$os" in
    Linux) os_part="unknown-linux-gnu" ;;
    Darwin) os_part="apple-darwin" ;;
    *) die "unsupported system '$os'. On Windows use the setup .exe from the release page." ;;
  esac
  case "$arch" in
    x86_64 | amd64) arch_part="x86_64" ;;
    aarch64 | arm64) arch_part="aarch64" ;;
    *) die "unsupported CPU '$arch'" ;;
  esac
  target="$arch_part-$os_part"
fi

bin_dir="${CRABFARM_BIN_DIR:-$HOME/.local/bin}"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/crabfarm-install.XXXXXX") || die "cannot create a temporary directory"
trap 'rm -rf "$tmp"' EXIT INT TERM

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    die "neither sha256sum nor shasum is available to verify the download"
  fi
}

fetch() { # url, destination
  if command -v curl >/dev/null 2>&1; then
    curl --fail --silent --show-error --location --retry 3 --max-time 300 --output "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget --quiet --tries=3 --timeout=60 --output-document="$2" "$1"
  else
    die "need curl or wget to download"
  fi
}

# Check "$1" (a file) against the entry for its own file name in SUMS file "$2".
verify() {
  name=$(basename "$1")
  want=$(grep -E "[[:space:]]\*?$name\$" "$2" | head -n 1 | cut -d' ' -f1 || true)
  [ -n "$want" ] || die "$name is not listed in $(basename "$2"); refusing to install it"
  got=$(sha256_of "$1")
  [ "$got" = "$want" ] || die "checksum mismatch for $name (expected $want, got $got); nothing installed"
  say "Checksum OK: $name"
}

if [ "$#" -ge 1 ]; then
  archive="$1"
  [ -f "$archive" ] || die "no such file: $archive"
  sums="$(dirname "$archive")/SHA256SUMS.txt"
  if [ -f "$sums" ]; then
    verify "$archive" "$sums"
  elif [ "${CRABFARM_SKIP_VERIFY:-}" = "1" ]; then
    say "Warning: installing $archive without checking a checksum (CRABFARM_SKIP_VERIFY=1)."
  else
    die "no SHA256SUMS.txt next to $archive. Download it from the same release, or set CRABFARM_SKIP_VERIFY=1."
  fi
else
  version="${CRABFARM_VERSION:-}"
  if [ -n "${CRABFARM_BASE_URL:-}" ]; then
    base="$CRABFARM_BASE_URL"
  elif [ -n "$version" ]; then
    base="https://github.com/$REPO/releases/download/v${version#v}"
  else
    base="https://github.com/$REPO/releases/latest/download"
  fi
  say "Downloading the checksum list ..."
  fetch "$base/SHA256SUMS.txt" "$tmp/SHA256SUMS.txt" || die "could not download $base/SHA256SUMS.txt"
  # The archive name carries the version; take it from the checksum list.
  name=$(grep -oE "crabfarm-[^[:space:]]+-$target\\.tar\\.gz" "$tmp/SHA256SUMS.txt" | head -n 1 || true)
  [ -n "$name" ] || die "this release has no package for $target"
  say "Downloading $name ..."
  fetch "$base/$name" "$tmp/$name" || die "could not download $base/$name"
  archive="$tmp/$name"
  verify "$archive" "$tmp/SHA256SUMS.txt"
fi

mkdir "$tmp/x"
tar -xzf "$archive" -C "$tmp/x" || die "could not unpack $archive"
cli=$(find "$tmp/x" -type f -name crabfarm | head -n 1)
daemon=$(find "$tmp/x" -type f -name crabfarmd | head -n 1)
[ -n "$cli" ] || die "the archive has no crabfarm program"
[ -n "$daemon" ] || die "the archive has no crabfarmd program (a release older than the installer?)"

mkdir -p "$bin_dir" || die "cannot create $bin_dir"
for pair in "$cli:crabfarm" "$daemon:crabfarmd"; do
  src=${pair%%:*}
  dst="$bin_dir/${pair##*:}"
  # Copy beside, then rename: a running node keeps its old file until it restarts.
  cp "$src" "$dst.new" && chmod 755 "$dst.new" && mv -f "$dst.new" "$dst" || die "could not install $dst"
done

say ""
say "Installed crabfarm and crabfarmd into $bin_dir"
case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) say "$bin_dir is not on your PATH yet. Add this line to ~/.profile, ~/.zshrc or ~/.bashrc:"
     say "  export PATH=\"$bin_dir:\$PATH\"" ;;
esac
say ""
say "Next steps:"
say "  crabfarm farm create          on the first machine (or: crabfarm farm join <code> on the others)"
say "  crabfarm service install      start the node at login (no sudo)"
say "  crabfarm enable               in a Rust project, so plain cargo build uses the farm"
say "Guide: https://crabfarm.renner.dev/guide.html"
say "If a node is already running from an older install, restart it: crabfarm node --stop, then crabfarm service install"
