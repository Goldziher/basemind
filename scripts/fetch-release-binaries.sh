#!/usr/bin/env bash
# Download checksum-verified basemind release binaries for tests/upgrade_from_release.rs.
#
# usage: scripts/fetch-release-binaries.sh <out-dir> <target-triple> <version|latest>...
#
# Prints the extracted binaries as one path list (the platform's PATH separator) on stdout, ready for
# BASEMIND_UPGRADE_FROM_BIN; progress goes to stderr. Fails on any missing asset, missing checksum
# entry or checksum mismatch, never installs an unverified binary.
#
# A version listed in pinned_sha256 must match the hash recorded here, not only the checksums file
# of its own release: that file comes from the same place as the archive, so it proves integrity,
# not provenance. A cached extraction is reused only when its recorded archive hash still matches
# and the binary still reports its version.
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
  local eff version
  eff="$(curl -fsSL --retry 3 -o /dev/null -w '%{url_effective}' "$BASE/latest")" ||
    die "could not resolve the latest release"
  case "$eff" in
  */releases/tag/v*) version="${eff##*/tag/v}" ;;
  *) die "unexpected redirect target for the latest release: $eff" ;;
  esac
  [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$ ]] || die "latest release has an unexpected version: $version"
  printf '%s\n' "$version"
}

# The archive hash this repository records for <version> <asset>, or nothing for an unpinned one.
pinned_sha256() {
  case "$1 $2" in
  "0.28.0 basemind-x86_64-unknown-linux-gnu.tar.gz") echo 1916727846e88fad80aee2af68141e63389fecca8f791c2c2defb95f81e6b5f1 ;;
  "0.28.0 basemind-aarch64-unknown-linux-gnu.tar.gz") echo c2e2000c3905eaf8ced7c1ff8707a903080c720e13367b0327bc8f43a62c8029 ;;
  "0.28.0 basemind-aarch64-apple-darwin.tar.gz") echo 420faad446d2712440df42fccb60ac05ee8523ea9eaf6a5a17a92e0a8fcd4b2a ;;
  esac
}

# Prints the version a binary reports, run in the sandbox.
reported_version() {
  env -u LD_LIBRARY_PATH BASEMIND_DATA_HOME="$SANDBOX/data" BASEMIND_COMMS_DIR="$SANDBOX/comms" \
    "$1" --version 2>"$SANDBOX/version.err"
}

# The sandbox every release binary is run in: a fresh data home and comms dir, so even `--version`
# cannot reach a real session's daemon or index.
SANDBOX="$(mktemp -d)"
stage=""
trap 'rm -rf "$SANDBOX" "$stage"' EXIT

mkdir -p "$OUT"
paths=()
for requested in "$@"; do
  version="$requested"
  [ "$requested" = "latest" ] && version="$(latest_version)"
  [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$ ]] || die "not a release version: $version"
  dir="$OUT/$version"
  pinned="$(pinned_sha256 "$version" "$ASSET")"
  if [ -f "$dir/.archive-sha256" ] && [ -x "$dir/basemind" ] && [ ! -L "$dir/basemind" ]; then
    recorded="$(cat "$dir/.archive-sha256")"
    if { [ -z "$pinned" ] || [ "$recorded" = "$pinned" ]; } &&
      [ "$(reported_version "$dir/basemind" || true)" = "basemind $version" ]; then
      log "$version already extracted and verified"
      paths+=("$dir/basemind")
      continue
    fi
    log "$version cache does not verify; fetching it again"
  fi
  # Download and extract into a staging directory beside the cache entry; the entry is replaced
  # only once everything below has passed.
  stage="$(mktemp -d "$OUT/.stage-$version.XXXXXX")"
  log "downloading $ASSET for v$version"
  curl -fsSL --retry 3 -o "$stage/$ASSET" "$BASE/download/v$version/$ASSET" || die "could not download $ASSET for v$version"
  curl -fsSL --retry 3 -o "$stage/checksums.txt" "$BASE/download/v$version/basemind_${version}_checksums.txt" ||
    die "could not download the checksums for v$version"
  expected="$(awk -v f="$ASSET" '{name=$NF; sub(/^[*]/, "", name); if (name == f) print $1}' "$stage/checksums.txt")"
  [ -n "$expected" ] || die "no checksum entry for $ASSET in v$version"
  actual="$(sha256 "$stage/$ASSET")"
  [ "$expected" = "$actual" ] || die "checksum mismatch for $ASSET v$version (expected $expected, got $actual)"
  if [ -n "$pinned" ] && [ "$pinned" != "$actual" ]; then
    die "$ASSET v$version does not match the hash this repository pins ($pinned, got $actual)"
  fi
  log "v$version checksum verified${pinned:+ against the pinned hash}"
  tar -xzf "$stage/$ASSET" -C "$stage"
  [ -f "$stage/basemind" ] && [ ! -L "$stage/basemind" ] || die "basemind not found as a regular file in $ASSET v$version"
  chmod +x "$stage/basemind"
  # A Linux release loads the libraries bundled in its lib/ through its RUNPATH, which
  # LD_LIBRARY_PATH outranks: a job that exports one for its own build (CI's libheif under
  # /usr/local/lib) would mix that build's libraries with the bundled libstdc++, and the loader
  # refuses. tests/upgrade_from_release.rs runs it with the variable removed as well.
  if ! reported="$(reported_version "$stage/basemind")"; then
    die "v$version $TRIPLE binary cannot run here: $(head -n 1 "$SANDBOX/version.err")"
  fi
  [ "$reported" = "basemind $version" ] || die "v$version binary reports \"$reported\""
  printf '%s\n' "$actual" >"$stage/.archive-sha256"
  rm -rf "$dir"
  mv "$stage" "$dir"
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
