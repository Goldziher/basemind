#!/usr/bin/env bash
# basemind MCP launcher — ensures a version-matched basemind binary is available,
# then exec's it with the forwarded arguments (the plugin passes `serve`).
#
# Why this exists: the Claude Code plugin ships manifests + scripts, not a
# compiled binary. Rather than require users to install basemind first, this
# launcher locates or installs a version-matched binary on first run, preferring
# tools the user likely already has, in this order:
#
#   1. An existing version-matched binary (cached in the plugin, or on PATH from
#      a prior brew/cargo/global-npm install) — fastest, no network.
#   2. `npx`  — runs the published npm package, which self-installs the binary.
#   3. `uvx`  — runs the published PyPI package, which self-installs the binary.
#   4. Direct download of the prebuilt release binary from GitHub (last resort,
#      for hosts with neither node nor uv), verified against release checksums.
#
# Override the selection with BASEMIND_LAUNCHER=auto|npx|uvx|download (default auto).
#
# CRITICAL: stdout is the MCP stdio protocol channel. Every diagnostic in this
# script MUST go to stderr (>&2). Only the exec'd binary may write to stdout.
set -euo pipefail

log() { printf 'basemind-launch: %s\n' "$*" >&2; }
die() {
  log "error: $*"
  exit 1
}

LAUNCHER="${BASEMIND_LAUNCHER:-auto}"

# Resolve the plugin root: prefer the value Claude Code injects, else derive it
# from this script's location (scripts/ lives one level under the plugin root).
PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-}"
if [ -z "$PLUGIN_ROOT" ]; then
  PLUGIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fi

BINARY_NAME="basemind"
case "$(uname -s)" in
MINGW* | MSYS* | CYGWIN* | Windows_NT) BINARY_NAME="basemind.exe" ;;
esac
BIN_DIR="$PLUGIN_ROOT/bin"
BIN="$BIN_DIR/$BINARY_NAME"

# Desired version = the plugin's declared version (single source of truth).
MANIFEST="$PLUGIN_ROOT/.claude-plugin/plugin.json"
[ -f "$MANIFEST" ] || die "plugin manifest not found at $MANIFEST"
VERSION="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$MANIFEST" | head -n1)"
[ -n "$VERSION" ] || die "could not read version from $MANIFEST"
# PyPI normalizes pre-release tags: 0.2.1-rc.1 (npm/cargo) -> 0.2.1rc1 (PyPI).
PYPI_VERSION="${VERSION//-rc./rc}"

# Return the X.Y.Z reported by a basemind binary, or empty if it can't run.
binary_version() { "$1" --version 2>/dev/null | awk '{print $2}'; }

have() { command -v "$1" >/dev/null 2>&1; }

# ---- 1. Existing version-matched binary (cached or on PATH) -----------------
# The version check also rejects a stale binary (e.g. an old global cargo
# install), so we never serve mismatched code.
if [ "$LAUNCHER" != "npx" ] && [ "$LAUNCHER" != "uvx" ] && [ "$LAUNCHER" != "download" ]; then
  if [ -x "$BIN" ] && [ "$(binary_version "$BIN")" = "$VERSION" ]; then
    exec "$BIN" "$@"
  fi
  if have "$BINARY_NAME"; then
    PATH_BIN="$(command -v "$BINARY_NAME")"
    if [ "$(binary_version "$PATH_BIN")" = "$VERSION" ]; then
      exec "$PATH_BIN" "$@"
    fi
  fi
fi

# ---- 2. npx (published npm package self-installs the binary) ----------------
# npx resolves a same-named local package.json before the registry, so in a repo
# named "basemind" (e.g. dogfooding, or this very repo) `npx basemind` finds the
# local package with no bin and fails. Run npx from a scratch cwd to dodge that,
# and hand basemind the real workspace via its global `--root` option.
if { [ "$LAUNCHER" = "auto" ] || [ "$LAUNCHER" = "npx" ]; } && have npx; then
  log "launching via npx basemind@$VERSION"
  REPO="$PWD"
  cd "$(mktemp -d)"
  exec npx -y "basemind@$VERSION" --root "$REPO" "$@"
fi

# ---- 3. uvx (published PyPI package self-installs the binary) ---------------
if { [ "$LAUNCHER" = "auto" ] || [ "$LAUNCHER" = "uvx" ]; } && have uvx; then
  log "launching via uvx basemind==$PYPI_VERSION"
  exec uvx --from "basemind==$PYPI_VERSION" basemind "$@"
fi

# ---- 4. Direct prebuilt download (last resort) ------------------------------
# Map uname → goreleaser target triple (matches npm-package/install.js).
arch="$(uname -m)"
case "$(uname -s)" in
Darwin)
  case "$arch" in
  arm64 | aarch64) TRIPLE="aarch64-apple-darwin" ;;
  *) TRIPLE="x86_64-apple-darwin" ;;
  esac
  ;;
Linux)
  case "$arch" in
  aarch64 | arm64) TRIPLE="aarch64-unknown-linux-gnu" ;;
  *) TRIPLE="x86_64-unknown-linux-gnu" ;;
  esac
  ;;
MINGW* | MSYS* | CYGWIN* | Windows_NT) TRIPLE="x86_64-pc-windows-msvc" ;;
*) die "unsupported platform: $(uname -s) $arch" ;;
esac
case "$TRIPLE" in
*windows*) EXT="zip" ;;
*) EXT="tar.gz" ;;
esac

BASE_URL="https://github.com/Goldziher/basemind/releases/download/v${VERSION}"
ASSET="basemind-${TRIPLE}.${EXT}"
ASSET_URL="${BASE_URL}/${ASSET}"
SUMS_URL="${BASE_URL}/basemind_${VERSION}_checksums.txt"

if have curl; then
  fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif have wget; then
  fetch() { wget -q -O "$2" "$1"; }
else
  die "no install method available: need npx, uvx, curl, or wget"
fi

if have sha256sum; then
  sha256() { sha256sum "$1" | awk '{print $1}'; }
elif have shasum; then
  sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  # Fail CLOSED: without a sha256 tool we cannot verify the download.
  die "no sha256 tool available (need sha256sum or shasum) — refusing to install unverified binary"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

log "no managed runtime found; downloading $ASSET ..."
fetch "$ASSET_URL" "$TMP/$ASSET" || die "download failed: $ASSET_URL"

# Fail CLOSED: the checksums file MUST be fetchable and MUST contain an entry
# for this asset. A missing file or absent entry aborts the install rather than
# proceeding with an unverified binary.
fetch "$SUMS_URL" "$TMP/checksums.txt" ||
  die "could not fetch checksums ($SUMS_URL) — refusing to install unverified binary"
EXPECTED="$(awk -v f="$ASSET" '$2 == f {print $1}' "$TMP/checksums.txt")"
[ -n "$EXPECTED" ] ||
  die "no checksum entry for $ASSET in $SUMS_URL — refusing to install unverified binary"
ACTUAL="$(sha256 "$TMP/$ASSET")"
[ -n "$ACTUAL" ] || die "failed to compute sha256 for $ASSET"
[ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch for $ASSET (expected $EXPECTED, got $ACTUAL)"
log "checksum verified"

log "extracting ..."
EX="$TMP/extracted"
mkdir -p "$EX"
case "$EXT" in
tar.gz) tar -xzf "$TMP/$ASSET" -C "$EX" ;;
zip)
  if have unzip; then
    unzip -qo "$TMP/$ASSET" -d "$EX"
  else
    die "need unzip to extract $ASSET"
  fi
  ;;
esac
# Archives now carry the binary plus a lib/ tree of bundled native libraries
# (found via rpath; Windows co-locates DLLs next to the exe). Install the whole
# extracted tree, not just the bare binary.
[ -f "$EX/$BINARY_NAME" ] || die "binary $BINARY_NAME not found in $ASSET"

rm -rf "$BIN_DIR"
mkdir -p "$BIN_DIR"
# Move every extracted entry (binary + lib/) into BIN_DIR.
mv "$EX"/* "$BIN_DIR"/
chmod +x "$BIN"
log "installed basemind $VERSION to $BIN"

exec "$BIN" "$@"
