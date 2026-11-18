#!/bin/sh
# Offline test of scripts/install.sh: fake programs in a fake archive, a temp
# HOME and bin dir. Touches nothing outside the temp directory.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/crabfarm-install-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM
export HOME="$tmp/home"
mkdir -p "$HOME"
target=x86_64-unknown-linux-gnu
name="crabfarm-9.9.9-$target"

fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

mkdir -p "$tmp/rel/$name"
printf '#!/bin/sh\necho crabfarm 9.9.9\n' > "$tmp/rel/$name/crabfarm"
printf '#!/bin/sh\necho crabfarmd\n' > "$tmp/rel/$name/crabfarmd"
tar -C "$tmp/rel" -czf "$tmp/rel/$name.tar.gz" "$name"
rm -rf "$tmp/rel/$name"
printf '%s  %s\n' "$(sha "$tmp/rel/$name.tar.gz")" "$name.tar.gz" > "$tmp/rel/SHA256SUMS.txt"

run() { CRABFARM_TARGET=$target CRABFARM_BIN_DIR="$tmp/bin" sh "$here/install.sh" "$@"; }

echo "1. local archive with checksum"
run "$tmp/rel/$name.tar.gz" > "$tmp/out1" 2>&1 || { cat "$tmp/out1"; fail "install failed"; }
[ -x "$tmp/bin/crabfarm" ] && [ -x "$tmp/bin/crabfarmd" ] || fail "programs not installed"
[ "$("$tmp/bin/crabfarm")" = "crabfarm 9.9.9" ] || fail "wrong program installed"
grep -q "not on your PATH" "$tmp/out1" || fail "no PATH hint"

echo "2. download through a base URL"
rm -rf "$tmp/bin"
# Git Bash on Windows hands a native curl a drive path, not /tmp/...
up=$(cygpath -m "$tmp/rel" 2>/dev/null || echo "$tmp/rel")
case "$up" in /*) url="file://$up" ;; *) url="file:///$up" ;; esac
CRABFARM_BASE_URL="$url" run > "$tmp/out2" 2>&1 || { cat "$tmp/out2"; fail "download install failed"; }
[ -x "$tmp/bin/crabfarmd" ] || fail "download: not installed"

echo "3. a wrong checksum installs nothing"
rm -rf "$tmp/bin"
printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" "$name.tar.gz" > "$tmp/rel/SHA256SUMS.txt"
if run "$tmp/rel/$name.tar.gz" > "$tmp/out3" 2>&1; then fail "a bad checksum was accepted"; fi
grep -q "checksum mismatch" "$tmp/out3" || fail "no mismatch message"
[ ! -e "$tmp/bin/crabfarm" ] || fail "installed despite a bad checksum"

echo "4. no checksum list refuses a local archive"
rm "$tmp/rel/SHA256SUMS.txt"
if run "$tmp/rel/$name.tar.gz" > "$tmp/out4" 2>&1; then fail "installed without any checksum"; fi
[ ! -e "$tmp/bin/crabfarm" ] || fail "installed without a checksum"

echo "5. no profile file was written"
[ -z "$(ls -A "$HOME")" ] || fail "install.sh wrote into HOME: $(ls -A "$HOME")"

echo "install.sh tests passed."
