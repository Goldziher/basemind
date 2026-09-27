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

# BASEMIND_FORCE_VERSION pins the launcher to a specific version instead of the ~keep
# manifest's. It is set only by the release-window fallback below, which re-execs this ~keep
# script to install the latest PUBLISHED release through the normal checksum-verified path. ~keep
FORCED_VERSION="${BASEMIND_FORCE_VERSION:-}"
if [ -n "$FORCED_VERSION" ]; then
  VERSION="$FORCED_VERSION"
else
  # The same launcher ships under every harness's plugin mirror; each mirror includes ~keep
  # only its own manifest (the Codex package excludes the Claude one, and vice versa). Probe ~keep
  # the known manifest locations so the launcher is harness-agnostic. Versions are lock-step, ~keep
  # so whichever exists yields the same pinned version. ~keep
  MANIFEST=""
  for cand in .claude-plugin/plugin.json .codex-plugin/plugin.json .cursor-plugin/plugin.json; do
    if [ -f "$PLUGIN_ROOT/$cand" ]; then
      MANIFEST="$PLUGIN_ROOT/$cand"
      break
    fi
  done
  [ -n "$MANIFEST" ] || die "plugin manifest not found under $PLUGIN_ROOT (.claude-plugin/, .codex-plugin/, or .cursor-plugin/)"
  VERSION="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$MANIFEST" | head -n1)"
  [ -n "$VERSION" ] || die "could not read version from $MANIFEST"
fi

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

# True when version $1 is at least version $2.
version_ge() { [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -n1)" = "$2" ]; }

# Floor for every FALLBACK path below. Versions under this have no bound on the comms daemon's ~keep
# resident set: the warm read-stack cap, the per-workspace byte budgets and the footprint gate all ~keep
# landed in 0.26.0 as issue #62, where a daemon reached 43.8 GiB RSS in about two minutes on an ~keep
# initial scan and was OOM-killed. That daemon is machine-global and outlives the session that ~keep
# spawned it, so execing one of these does not degrade a session — it takes down the machine, and ~keep
# it keeps doing so for as long as any session re-ensures it. A missing MCP server is strictly ~keep
# better, so refuse rather than fall back. Does not gate VERSION itself or BASEMIND_BIN: those are ~keep
# explicit pins, not the launcher choosing on the user's behalf. ~keep
MIN_SAFE_VERSION="0.26.0"

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
    version_ge "$ver" "$MIN_SAFE_VERSION" || continue
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

# BASEMIND_BIN is an explicit operator pin — honour it whatever version it reports, unlike every ~keep
# resolution step below. Gating it on VERSION made it useless for the case it exists for: a local ~keep
# `cargo build` is almost never the version the installed plugin manifest pins, so the gate silently ~keep
# ignored the override and went off to download a release binary instead. A mismatched schema minor ~keep
# triggers a wipe-and-rebuild of the caches, which is why the version is still reported. ~keep
if [ -n "${BASEMIND_BIN:-}" ] && [ -x "${BASEMIND_BIN}" ]; then
  bin_ver="$(binary_version "$BASEMIND_BIN")"
  if [ "$bin_ver" != "$VERSION" ]; then
    log "BASEMIND_BIN pins basemind ${bin_ver:-unknown} while the plugin manifest pins $VERSION; using the pin"
  fi
  exec "$BASEMIND_BIN" "$@"
fi
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

# Newest PUBLISHED release tag ("vX.Y.Z"), or empty. GitHub exposes only non-draft, non-prerelease ~keep
# releases at /releases/latest. That makes it the best FIRST candidate but NOT a guarantee of ~keep
# completeness: the publish workflow promotes a draft only after every asset exists, yet a human ~keep
# can promote it by hand first, and for v0.27.0 someone did — `latest` carried 3 of 5 archives and ~keep
# no checksums file. Everything downstream therefore gates on `release_is_installable`, never on ~keep
# publication status. Uses curl's redirect target (no API rate limit) when available, else the ~keep
# releases API (works with curl or wget). ~keep
resolve_latest_tag() {
  local eff tmp
  if have curl; then
    eff="$(curl -fsSL -o /dev/null -w '%{url_effective}' \
      "https://github.com/Goldziher/basemind/releases/latest" 2>/dev/null || true)"
    case "$eff" in
    */releases/tag/*)
      printf '%s\n' "${eff##*/tag/}"
      return 0
      ;;
    esac
  fi
  tmp="$(mktemp)"
  if fetch "https://api.github.com/repos/Goldziher/basemind/releases/latest" "$tmp" 2>/dev/null; then
    sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$tmp" | head -n1
  fi
  rm -f "$tmp" 2>/dev/null || true
}

# Published, non-draft versions newest-first, up to 20 (bare "X.Y.Z", no leading "v"). Reached ~keep
# only on the fallback path, so the unauthenticated API rate limit is never on the hot path. May ~keep
# include prereleases, which /releases/latest excludes; that is acceptable here because every ~keep
# candidate still has to clear MIN_SAFE_VERSION and release_is_installable, and a prerelease that ~keep
# clears both is a working binary — strictly better than no MCP server at all. ~keep
published_versions() {
  local tmp
  tmp="$(mktemp)"
  if fetch "https://api.github.com/repos/Goldziher/basemind/releases?per_page=20" "$tmp" 2>/dev/null; then
    sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' "$tmp"
  fi
  rm -f "$tmp" 2>/dev/null || true
}

# True when v$1 carries a checksums file that lists THIS platform's asset — precisely the ~keep
# precondition the install path below needs, checked directly instead of inferred from the ~keep
# release's published flag. One fetch per candidate, on the failure path only. ~keep
release_is_installable() {
  local ver="$1" tmp rc=1
  tmp="$(mktemp)"
  if fetch "https://github.com/Goldziher/basemind/releases/download/v${ver}/basemind_${ver}_checksums.txt" "$tmp" 2>/dev/null &&
    awk -v f="$ASSET" '{name=$NF; sub(/^[*]/, "", name); if (name == f) found=1} END{exit !found}' "$tmp"; then
    rc=0
  fi
  rm -f "$tmp" 2>/dev/null || true
  return "$rc"
}

# The pinned v${VERSION} assets are missing (release still publishing, or genuinely ~keep
# incomplete). Keep the session working instead of leaving it with no MCP server: ~keep
#   1. Re-exec pinned to the newest release whose assets actually exist; Claude Code ~keep
#      auto-updates to v${VERSION} once it publishes. Skipped when already running a forced ~keep
#      version, so it can never loop. ~keep
#   2. Offline last resort: newest cached binary of the same schema minor. ~keep
#   3. Give up with the incomplete-release message. ~keep
#
# Steps 1 and 2 both refuse anything below MIN_SAFE_VERSION. A fallback exists to preserve a ~keep
# session's tooling, and no amount of tooling is worth OOM-ing the machine. ~keep
fallback_or_die() {
  local reason="$1" cand fb tried="" attempts=0
  if [ -z "$FORCED_VERSION" ]; then
    # Newest-first, `latest` before the rest because it is the only candidate GitHub guarantees ~keep
    # is neither a draft nor a prerelease. Several candidates rather than just `latest`: `latest` ~keep
    # is exactly what was broken in the v0.27.0 incident, and with one candidate a bad `latest` ~keep
    # left every session with no server at all. ~keep
    for cand in "$(resolve_latest_tag | sed 's/^v//')" $(published_versions); do
      [ -n "$cand" ] || continue
      [ "$cand" = "$VERSION" ] && continue
      case " $tried " in *" $cand "*) continue ;; esac
      tried="$tried $cand"
      version_ge "$cand" "$MIN_SAFE_VERSION" || continue
      attempts=$((attempts + 1))
      [ "$attempts" -gt 5 ] && break
      release_is_installable "$cand" || continue
      log "warning: $reason"
      log "v${VERSION} is not installable; running basemind ${cand} instead — Claude Code will auto-update to v${VERSION} once its release completes"
      release_lock
      LOCK_HELD=""
      exec env BASEMIND_FORCE_VERSION="$cand" "$0" "${ARGS[@]}"
    done
  fi
  fb="$(newest_compatible_cached)"
  if [ -n "$fb" ]; then
    log "warning: $reason"
    log "v${VERSION} not yet downloadable; falling back to compatible cached basemind $(binary_version "$fb") at $fb — run \`/plugin update\` once the release completes"
    release_lock
    LOCK_HELD=""
    reap_other_versions "$(dirname "$fb")"
    cleanup_stale_state
    exec "$fb" "${ARGS[@]}"
  fi
  die_incomplete_release "$reason"
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
