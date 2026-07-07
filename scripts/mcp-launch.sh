#!/usr/bin/env bash
# basemind MCP launcher — ensures a version-matched basemind binary is available,
# then exec's it with the forwarded arguments (the plugin passes `serve`).
#
# Why this exists: the Claude Code / Cursor / Gemini plugins ship manifests +
# scripts, not a compiled binary. This launcher installs a version-matched
# prebuilt binary on first run and exec's it directly on every run thereafter.
#
# Install model (single method, by design):
#
#   1. A version-matched binary already present — the managed cache, a pre-seeded
#      plugin `bin/`, an explicit $BASEMIND_BIN, or one on PATH. Fastest, no network.
#   2. Otherwise, download the prebuilt release binary from GitHub, verify it
#      against the release checksums, install it into a stable per-user cache, and
#      exec it. Concurrent launches serialize on a lock; the download happens once
#      per version per machine.
#
# Why not npx/uvx: earlier revisions exec'd `npx basemind@VERSION` / `uvx ...` as
# the runtime. npx stages into a shared, spec-hashed `_npx/<hash>` dir, so two
# concurrent basemind launches (multiple agent sessions, or the comms-monitor
# poll loop) raced on it and failed with `ENOENT package.json`. It also never
# populated the fast-path cache (so every launch re-resolved over the network) and
# inherited node/python startup cost plus lavamoat postinstall blocks. A direct,
# checksum-verified download to a stable cache has none of those failure modes.
#
# Override the binary with BASEMIND_BIN=/path/to/basemind (e.g. a local dev build).
#
# CRITICAL: stdout is the MCP stdio protocol channel. Every diagnostic in this
# script MUST go to stderr (>&2). Only the exec'd binary may write to stdout.
set -euo pipefail

log() { printf 'basemind-launch: %s\n' "$*" >&2; }
die() {
	log "error: $*"
	exit 1
}

# A failed asset or checksums fetch almost always means the pinned release is
# INCOMPLETE — some platform binaries or the checksums file never finished
# publishing (a partial release). Surface that as an actionable instruction
# instead of a bare error the MCP client renders as an opaque "failed to connect".
die_incomplete_release() {
	die "$1 — the basemind v${VERSION} release looks incomplete (a missing platform asset or checksums file). Update the basemind plugin to a complete release (Claude Code: run \`/plugin update\`); if it persists, report it at https://github.com/Goldziher/basemind/issues"
}

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

# Desired version = the plugin's declared version (single source of truth).
MANIFEST="$PLUGIN_ROOT/.claude-plugin/plugin.json"
[ -f "$MANIFEST" ] || die "plugin manifest not found at $MANIFEST"
VERSION="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$MANIFEST" | head -n1)"
[ -n "$VERSION" ] || die "could not read version from $MANIFEST"

# Stable per-user, per-version install dir — downloaded once per machine and
# shared across every launcher invocation (plugin snapshot, repo comms-monitor,
# other repos). Lives outside any git working tree and survives plugin updates.
CACHE_ROOT="${XDG_CACHE_HOME:-$HOME/.cache}/basemind/bin/$VERSION"
MANAGED_BIN="$CACHE_ROOT/$BINARY_NAME"
PARENT="$(dirname "$CACHE_ROOT")"

# Return the X.Y.Z reported by a basemind binary, or empty if it can't run.
binary_version() { "$1" --version 2>/dev/null | awk '{print $2}'; }
have() { command -v "$1" >/dev/null 2>&1; }

# Remove stale per-version install dirs so old binaries don't accrue in the cache
# (users otherwise pile up a dir per release under .cache/basemind/bin). The
# launcher only ever execs the version-matched binary, so any X.Y.Z-named sibling
# that isn't $VERSION is dead weight. Best-effort, and invoked ONLY once a $VERSION
# binary is confirmed present (fast-path match or fresh install) so we never delete
# a user's only working copy before a replacement exists. A dir held open by a
# concurrent session of a different plugin version just fails to remove (harmless
# on Unix — the running process keeps its mapping) and is retried on a later launch.
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

# Exec the candidate (first arg) with the forwarded launcher args if it exists and
# its version matches the manifest. The candidate is shifted off before exec so it
# is not re-passed as an argument to itself.
try_exec() {
	local cand="$1"
	shift
	if [ -n "$cand" ] && [ -x "$cand" ] && [ "$(binary_version "$cand")" = "$VERSION" ]; then
		exec "$cand" "$@"
	fi
}

# ---- 1. Existing version-matched binary -------------------------------------
# Explicit override first (dev builds), then the managed cache, a pre-seeded
# plugin bin/, and finally a matching binary already on PATH (brew/cargo/npm).
try_exec "${BASEMIND_BIN:-}" "$@"
# Managed-cache fast path: prune stale versions before exec so an already-current
# user still gets accrued old dirs cleaned (no download needed to trigger cleanup).
if [ -x "$MANAGED_BIN" ] && [ "$(binary_version "$MANAGED_BIN")" = "$VERSION" ]; then
	prune_stale_versions
	exec "$MANAGED_BIN" "$@"
fi
try_exec "$PLUGIN_ROOT/bin/$BINARY_NAME" "$@"
if have "$BINARY_NAME"; then
	try_exec "$(command -v "$BINARY_NAME")" "$@"
fi

# ---- 2. Download the checksum-verified prebuilt release binary --------------
# Map uname → target triple (matches npm-package/install.js and the pip downloader).
arch="$(uname -m)"
case "$(uname -s)" in
Darwin)
	# Apple Silicon — including an x86_64 process under Rosetta, where `uname -m`
	# reports x86_64 on arm64 hardware — takes the NATIVE arm64 binary (it runs
	# natively even when exec'd from a translated parent). Either hardware signal is
	# conclusive for Apple Silicon:
	#   * `sysctl.proc_translated` = 1 → running under Rosetta, which exists ONLY on
	#     Apple Silicon. Rosetta MASKS `hw.optional.arm64`, so that check alone misses
	#     the Rosetta-shell case (the bug this line fixes).
	#   * `hw.optional.arm64` = 1 → native arm64 process.
	# A genuine Intel Mac matches neither and gets the x86_64 binary.
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
	# Fail CLOSED: without a sha256 tool we cannot verify the download.
	die "no sha256 tool available (need sha256sum or shasum) — refusing to install unverified binary"
fi

# Concurrency: many launchers may race the first install (agent sessions, the
# comms-monitor poll loop). Serialize with an atomic mkdir lock — portable, since
# flock is absent on macOS. The winner downloads; losers wait for the managed
# binary to appear, then exec it.
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
	# Another launcher is installing. The moment the managed binary lands, use it.
	try_exec "$MANAGED_BIN" "$@"
	sleep 0.2
	waited=$((waited + 1))
	# ~120s with no progress ⇒ assume a crashed holder and break the stale lock.
	if [ "$waited" -ge 600 ]; then
		rmdir "$LOCK" 2>/dev/null || true
		waited=0
	fi
done
LOCK_HELD=1

# Double-check under the lock: another launcher may have finished while we waited.
try_exec "$MANAGED_BIN" "$@"

TMP="$(mktemp -d)"
log "downloading $ASSET ..."
fetch "$ASSET_URL" "$TMP/$ASSET" || die_incomplete_release "could not download $ASSET ($ASSET_URL)"

# Fail CLOSED: the checksums file MUST be fetchable and MUST contain an entry for
# this asset. A missing file or absent entry aborts rather than installing an
# unverified binary — and almost always means the release published without its
# checksums (the v0.10.0 partial-publish failure mode), so point at the fix.
fetch "$SUMS_URL" "$TMP/checksums.txt" ||
	die_incomplete_release "could not fetch checksums ($SUMS_URL); refusing to install an unverified binary"
EXPECTED="$(awk -v f="$ASSET" '{name=$NF; sub(/^[*]/, "", name); if (name == f) print $1}' "$TMP/checksums.txt")"
[ -n "$EXPECTED" ] ||
	die_incomplete_release "no checksum entry for $ASSET in $SUMS_URL; refusing to install an unverified binary"
ACTUAL="$(sha256 "$TMP/$ASSET")"
[ -n "$ACTUAL" ] || die "failed to compute sha256 for $ASSET"
[ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch for $ASSET (expected $EXPECTED, got $ACTUAL)"
log "checksum verified"

# Extract into a staging dir on the SAME filesystem as the cache, so the final
# install is a single atomic rename (no window where a reader sees a half-tree).
# Archives carry the binary plus a lib/ tree of bundled native libraries (Windows
# co-locates DLLs next to the exe) — install the whole tree, not just the binary.
log "extracting ..."
STAGING="$PARENT/.staging-$VERSION-$$"
rm -rf "$STAGING"
mkdir -p "$STAGING"
case "$EXT" in
tar.gz) tar -xzf "$TMP/$ASSET" -C "$STAGING" ;;
zip)
	# Windows git-bash ships no `unzip`. Try it first, then bsdtar (Windows 10+
	# system tar.exe extracts zip), then PowerShell's Expand-Archive.
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

# Atomic swap into place. CACHE_ROOT is version-scoped, so on a fresh version it
# does not exist and this is a pure rename; only a partial/corrupt prior dir is
# cleared first (under the lock, so no concurrent installer collides).
[ -e "$CACHE_ROOT" ] && rm -rf "$CACHE_ROOT"
mv "$STAGING" "$CACHE_ROOT"
STAGING=""
log "installed basemind $VERSION to $CACHE_ROOT"

rm -rf "$TMP"
TMP=""
release_lock
LOCK_HELD=""

# The new version is now installed at $CACHE_ROOT — safe to reclaim old versions.
prune_stale_versions

exec "$MANAGED_BIN" "$@"
