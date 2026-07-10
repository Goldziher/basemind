#!/usr/bin/env bash
# basemind MCP launcher — ensures a version-matched basemind binary is available,
# scripts, not a compiled binary. This launcher installs a version-matched
set -euo pipefail

log() { printf 'basemind-launch: %s\n' "$*" >&2; }
die() {
	log "error: $*"
	exit 1
}

die_incomplete_release() {
	die "$1 — the basemind v${VERSION} release looks incomplete (a missing platform asset or checksums file). Update the basemind plugin to a complete release (Claude Code: run \`/plugin update\`); if it persists, report it at https://github.com/Goldziher/basemind/issues"
}

PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-}"
if [ -z "$PLUGIN_ROOT" ]; then
	PLUGIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fi

BINARY_NAME="basemind"
case "$(uname -s)" in
MINGW* | MSYS* | CYGWIN* | Windows_NT) BINARY_NAME="basemind.exe" ;;
esac

MANIFEST="$PLUGIN_ROOT/.claude-plugin/plugin.json"
[ -f "$MANIFEST" ] || die "plugin manifest not found at $MANIFEST"
VERSION="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$MANIFEST" | head -n1)"
[ -n "$VERSION" ] || die "could not read version from $MANIFEST"

CACHE_ROOT="${XDG_CACHE_HOME:-$HOME/.cache}/basemind/bin/$VERSION"
MANAGED_BIN="$CACHE_ROOT/$BINARY_NAME"
PARENT="$(dirname "$CACHE_ROOT")"

binary_version() { "$1" --version 2>/dev/null | awk '{print $2}'; }
have() { command -v "$1" >/dev/null 2>&1; }

prune_stale_versions() {
	[ -d "$PARENT" ] || return 0
	local entry base
	for entry in "$PARENT"/*/; do
		[ -d "$entry" ] || continue
		base="$(basename "$entry")"
		[ "$base" = "$VERSION" ] && continue
		case "$base" in
		[0-9]*) rm -rf "$entry" 2>/dev/null || true ;;
		esac
	done
}

try_exec() {
	local cand="$1"
	shift
	if [ -n "$cand" ] && [ -x "$cand" ] && [ "$(binary_version "$cand")" = "$VERSION" ]; then
		exec "$cand" "$@"
	fi
}

try_exec "${BASEMIND_BIN:-}" "$@"
if [ -x "$MANAGED_BIN" ] && [ "$(binary_version "$MANAGED_BIN")" = "$VERSION" ]; then
	prune_stale_versions
	exec "$MANAGED_BIN" "$@"
fi
try_exec "$PLUGIN_ROOT/bin/$BINARY_NAME" "$@"
if have "$BINARY_NAME"; then
	try_exec "$(command -v "$BINARY_NAME")" "$@"
fi

arch="$(uname -m)"
case "$(uname -s)" in
Darwin)
	if [ "$arch" = "arm64" ] || [ "$arch" = "aarch64" ] ||
		[ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = "1" ] ||
		[ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = "1" ]; then
		TRIPLE="aarch64-apple-darwin"
	else
		TRIPLE="x86_64-apple-darwin"
	fi
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
	die "no download tool available: need curl or wget"
fi

if have sha256sum; then
	sha256() { sha256sum "$1" | awk '{print $1}'; }
elif have shasum; then
	sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
else
	die "no sha256 tool available (need sha256sum or shasum) — refusing to install unverified binary"
fi

mkdir -p "$PARENT"
LOCK="$PARENT/.lock-$VERSION"
STAGING=""
release_lock() { [ -n "${LOCK_HELD:-}" ] && rmdir "$LOCK" 2>/dev/null || true; }
cleanup() {
	release_lock
	[ -n "${TMP:-}" ] && rm -rf "$TMP" 2>/dev/null || true
	[ -n "$STAGING" ] && rm -rf "$STAGING" 2>/dev/null || true
}
trap cleanup EXIT

LOCK_HELD=""
waited=0
while ! mkdir "$LOCK" 2>/dev/null; do
	try_exec "$MANAGED_BIN" "$@"
	sleep 0.2
	waited=$((waited + 1))
	if [ "$waited" -ge 600 ]; then
		rmdir "$LOCK" 2>/dev/null || true
		waited=0
	fi
done
LOCK_HELD=1

try_exec "$MANAGED_BIN" "$@"

TMP="$(mktemp -d)"
log "downloading $ASSET ..."
fetch "$ASSET_URL" "$TMP/$ASSET" || die_incomplete_release "could not download $ASSET ($ASSET_URL)"

fetch "$SUMS_URL" "$TMP/checksums.txt" ||
	die_incomplete_release "could not fetch checksums ($SUMS_URL); refusing to install an unverified binary"
EXPECTED="$(awk -v f="$ASSET" '{name=$NF; sub(/^[*]/, "", name); if (name == f) print $1}' "$TMP/checksums.txt")"
[ -n "$EXPECTED" ] ||
	die_incomplete_release "no checksum entry for $ASSET in $SUMS_URL; refusing to install an unverified binary"
ACTUAL="$(sha256 "$TMP/$ASSET")"
[ -n "$ACTUAL" ] || die "failed to compute sha256 for $ASSET"
[ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch for $ASSET (expected $EXPECTED, got $ACTUAL)"
log "checksum verified"

log "extracting ..."
STAGING="$PARENT/.staging-$VERSION-$$"
rm -rf "$STAGING"
mkdir -p "$STAGING"
case "$EXT" in
tar.gz) tar -xzf "$TMP/$ASSET" -C "$STAGING" ;;
zip)
	if have unzip; then
		unzip -qo "$TMP/$ASSET" -d "$STAGING"
	elif tar -xf "$TMP/$ASSET" -C "$STAGING" 2>/dev/null; then
		:
	elif have powershell; then
		powershell -NoProfile -Command \
			"Expand-Archive -Path '$TMP/$ASSET' -DestinationPath '$STAGING' -Force" ||
			die "Expand-Archive failed to extract $ASSET"
	else
		die "no zip extractor available (need unzip, bsdtar, or powershell)"
	fi
	;;
esac
[ -f "$STAGING/$BINARY_NAME" ] || die "binary $BINARY_NAME not found in $ASSET"
chmod +x "$STAGING/$BINARY_NAME"

[ -e "$CACHE_ROOT" ] && rm -rf "$CACHE_ROOT"
mv "$STAGING" "$CACHE_ROOT"
STAGING=""
log "installed basemind $VERSION to $CACHE_ROOT"

rm -rf "$TMP"
TMP=""
release_lock
LOCK_HELD=""

prune_stale_versions

exec "$MANAGED_BIN" "$@"
