#!/usr/bin/env bash
# Download checksum-verified basemind release binaries for tests/upgrade_from_release.rs.
#
# usage: scripts/fetch-release-binaries.sh <out-dir> <target-triple> <version|latest>...
#
# Prints the extracted binaries as one path list (the platform's PATH separator) on stdout, ready for
# BASEMIND_UPGRADE_FROM_BIN; progress goes to stderr. Fails on any missing asset, missing checksum
# entry or checksum mismatch, never installs an unverified binary.
set -euo pipefail

log() { printf 'fetch-release-binaries: %s\n' "$*" >&2; }
die() {
  log "error: $*"
  exit 1
}

[ "$#" -ge 3 ] || die "usage: $0 <out-dir> <target-triple> <version|latest>..."
OUT="$1"
TRIPLE="$2"
shift 2

case "$TRIPLE" in
*windows*) die "no Windows release binary is fetched: the upgrade test is not collected there" ;;
esac
ASSET="basemind-${TRIPLE}.tar.gz"
BASE="https://github.com/Goldziher/basemind/releases"

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  die "need sha256sum or shasum"
fi

latest_version() {
  local eff
  eff="$(curl -fsSL -o /dev/null -w '%{url_effective}' "$BASE/latest")" || die "could not resolve the latest release"
  case "$eff" in
  */releases/tag/v*) printf '%s\n' "${eff##*/tag/v}" ;;
  *) die "unexpected redirect target for the latest release: $eff" ;;
  esac
}

# The sandbox every release binary is run in: a fresh data home and comms dir, so even `--version`
# cannot reach a real session's daemon or index.
SANDBOX="$(mktemp -d)"
trap 'rm -rf "$SANDBOX"' EXIT

mkdir -p "$OUT"
paths=()
for requested in "$@"; do
  version="$requested"
  [ "$requested" = "latest" ] && version="$(latest_version)"
  dir="$OUT/$version"
  if [ -x "$dir/basemind" ]; then
    log "$version already extracted"
    paths+=("$dir/basemind")
    continue
  fi
  mkdir -p "$dir"
  log "downloading $ASSET for v$version"
  curl -fsSL --retry 3 -o "$dir/$ASSET" "$BASE/download/v$version/$ASSET" || die "could not download $ASSET for v$version"
  curl -fsSL --retry 3 -o "$dir/checksums.txt" "$BASE/download/v$version/basemind_${version}_checksums.txt" ||
    die "could not download the checksums for v$version"
  expected="$(awk -v f="$ASSET" '{name=$NF; sub(/^[*]/, "", name); if (name == f) print $1}' "$dir/checksums.txt")"
  [ -n "$expected" ] || die "no checksum entry for $ASSET in v$version"
  actual="$(sha256 "$dir/$ASSET")"
  [ "$expected" = "$actual" ] || die "checksum mismatch for $ASSET v$version (expected $expected, got $actual)"
  log "v$version checksum verified"
  tar -xzf "$dir/$ASSET" -C "$dir"
  [ -f "$dir/basemind" ] || die "basemind not found in $ASSET v$version"
  chmod +x "$dir/basemind"
  # A Linux release loads the libraries bundled in its lib/ through its RUNPATH, which
  # LD_LIBRARY_PATH outranks: a job that exports one for its own build (CI's libheif under
  # /usr/local/lib) would mix that build's libraries with the bundled libstdc++, and the loader
  # refuses. tests/upgrade_from_release.rs runs it with the variable removed as well.
  if ! reported="$(env -u LD_LIBRARY_PATH BASEMIND_DATA_HOME="$SANDBOX/data" BASEMIND_COMMS_DIR="$SANDBOX/comms" \
    "$dir/basemind" --version 2>"$SANDBOX/version.err")"; then
    die "v$version $TRIPLE binary cannot run here: $(head -n 1 "$SANDBOX/version.err")"
  fi
  [ "$reported" = "basemind $version" ] || die "v$version binary reports \"$reported\""
  paths+=("$dir/basemind")
done

sep=":"
case "$(uname -s)" in
MINGW* | MSYS* | CYGWIN* | Windows_NT) sep=";" ;;
esac
list=""
for p in "${paths[@]}"; do
  list="${list:+$list$sep}$p"
done
printf '%s\n' "$list"
