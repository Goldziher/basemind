#!/usr/bin/env bash
# basemind MCP launcher — ensures a version-matched basemind binary is available,
# scripts, not a compiled binary. This launcher installs a version-matched
set -euo pipefail

# Original stdio args, forwarded to whichever binary we exec. Captured here because a ~keep
# fallback exec happens inside a function, where "$@" would mean the function's args. ~keep
ARGS=("$@")

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

# Terminate basemind processes (serve, comms/write/shell daemons — all the same binary) that ~keep
# belong to a DIFFERENT cached version than the one we are about to exec, so a version update ~keep
# converges the machine on a single generation instead of leaking orphans across sessions. Matches ~keep
# by argv[0] under the cache bin root; keeps everything under keep_dir. Unix-only and strictly ~keep
# best-effort — it never blocks or fails the launch (Windows has no reliable ps-based reap). ~keep
reap_other_versions() {
	local keep_dir="$1"
	case "$(uname -s)" in
	MINGW* | MSYS* | CYGWIN* | Windows_NT) return 0 ;;
	esac
	have ps || return 0
	local pids
	pids="$(ps -axo pid=,args= 2>/dev/null | awk -v root="$PARENT/" -v keep="$keep_dir/" -v me="$$" '
		{ pid = $1; path = $2 }
		pid == me { next }
		index(path, root) == 1 && index(path, keep) != 1 { print pid }')"
	[ -n "$pids" ] || return 0
	log "reaping stale-version basemind processes: $(printf '%s' "$pids" | tr '\n' ' ')"
	printf '%s\n' "$pids" | xargs kill -TERM 2>/dev/null || true
	local waited=0 alive p
	while [ "$waited" -lt 20 ]; do
		alive=0
		for p in $pids; do kill -0 "$p" 2>/dev/null && alive=1; done
		[ "$alive" -eq 0 ] && break
		sleep 0.1
		waited=$((waited + 1))
	done
	printf '%s\n' "$pids" | xargs kill -KILL 2>/dev/null || true
}

# Remove leftover install lock/staging dirs from other versions (crashed or superseded installs). ~keep
# The lock for VERSION is preserved — a concurrent session may be holding it to install right now. ~keep
cleanup_stale_state() {
	[ -d "$PARENT" ] || return 0
	local entry
	for entry in "$PARENT"/.lock-* "$PARENT"/.staging-*; do
		[ -e "$entry" ] || continue
		[ "$entry" = "$PARENT/.lock-$VERSION" ] && continue
		rm -rf "$entry" 2>/dev/null || true
	done
}

# Schema minor of a version string: 0.22.1 -> 0.22, 1.3.0-rc.2 -> 1.3. Binaries that ~keep
# share a minor share the blob + index schema (RELEASE_MINOR), so one can stand in for ~keep
# another; a different minor would trigger a wipe-and-rebuild and must never be a fallback. ~keep
version_minor() { printf '%s\n' "$1" | sed -n 's/^\([0-9][0-9]*\.[0-9][0-9]*\).*/\1/p'; }

# Newest cached binary whose schema minor matches VERSION (so it is blob/index ~keep
# compatible), excluding VERSION itself. Prints its path, or nothing. Used to keep the ~keep
# MCP server available during a release window, before the pinned assets finish uploading. ~keep
newest_compatible_cached() {
	[ -d "$PARENT" ] || return 0
	local want entry base bin ver best_ver="" best_bin=""
	want="$(version_minor "$VERSION")"
	for entry in "$PARENT"/*/; do
		[ -d "$entry" ] || continue
		base="$(basename "$entry")"
		case "$base" in
		[0-9]*) ;;
		*) continue ;;
		esac
		[ "$base" = "$VERSION" ] && continue
		bin="$entry$BINARY_NAME"
		[ -x "$bin" ] || continue
		ver="$(binary_version "$bin")"
		[ -n "$ver" ] || continue
		[ "$(version_minor "$ver")" = "$want" ] || continue
		if [ -z "$best_ver" ] || [ "$(printf '%s\n%s\n' "$best_ver" "$ver" | sort -V | tail -n1)" = "$ver" ]; then
			best_ver="$ver"
			best_bin="$bin"
		fi
	done
	[ -n "$best_bin" ] && printf '%s\n' "$best_bin"
	return 0
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
	reap_other_versions "$CACHE_ROOT"
	prune_stale_versions
	cleanup_stale_state
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

# The pinned v${VERSION} assets are missing (release still publishing, or genuinely ~keep
# incomplete). Rather than leave the session with no MCP server, exec the newest cached ~keep
# binary of the same schema minor if one exists; only give up when none is available. ~keep
fallback_or_die() {
	local fb
	fb="$(newest_compatible_cached)"
	if [ -n "$fb" ]; then
		log "warning: $1"
		log "v${VERSION} not yet downloadable; falling back to compatible cached basemind $(binary_version "$fb") at $fb — run \`/plugin update\` once the release completes"
		release_lock
		LOCK_HELD=""
		reap_other_versions "$(dirname "$fb")"
		cleanup_stale_state
		exec "$fb" "${ARGS[@]}"
	fi
	die_incomplete_release "$1"
}

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
fetch "$ASSET_URL" "$TMP/$ASSET" || fallback_or_die "could not download $ASSET ($ASSET_URL)"

fetch "$SUMS_URL" "$TMP/checksums.txt" ||
	fallback_or_die "could not fetch checksums ($SUMS_URL); refusing to install an unverified binary"
EXPECTED="$(awk -v f="$ASSET" '{name=$NF; sub(/^[*]/, "", name); if (name == f) print $1}' "$TMP/checksums.txt")"
[ -n "$EXPECTED" ] ||
	fallback_or_die "no checksum entry for $ASSET in $SUMS_URL; refusing to install an unverified binary"
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

reap_other_versions "$CACHE_ROOT"
prune_stale_versions
cleanup_stale_state

exec "$MANAGED_BIN" "$@"
