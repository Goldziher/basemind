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
# Set on Git Bash, MSYS2 and Cygwin, whose ps and paths differ from every Unix. ~keep
MSYS_SHELL=""
case "$(uname -s)" in
MINGW* | MSYS* | CYGWIN*)
  BINARY_NAME="basemind.exe"
  MSYS_SHELL=1
  ;;
Windows_NT) BINARY_NAME="basemind.exe" ;;
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

# Cached versions live under versions/. Launchers up to 0.33 keep theirs under the sibling bin/, ~keep
# where each of them stops every process started from another version's directory and deletes ~keep
# that directory. Nothing here installs into or launches from bin/, so those launchers never see ~keep
# this one's versions. While such a launcher is still in use it manages bin/ itself; this one only ~keep
# removes what is left there once nothing has used it for PRUNE_AFTER_DAYS. ~keep
PARENT="${XDG_CACHE_HOME:-$HOME/.cache}/basemind/versions"
LEGACY_PARENT="${XDG_CACHE_HOME:-$HOME/.cache}/basemind/bin"
CACHE_ROOT="$PARENT/$VERSION"
MANAGED_BIN="$CACHE_ROOT/$BINARY_NAME"

# Days a cached version must go unused before it may be pruned. ~keep
PRUNE_AFTER_DAYS="${BASEMIND_PRUNE_AFTER_DAYS:-14}"
case "$PRUNE_AFTER_DAYS" in
'' | *[!0-9]*) PRUNE_AFTER_DAYS=14 ;;
esac
# Minutes an install lock or staging directory must sit untouched before it counts as abandoned. ~keep
STALE_INSTALL_MINUTES=10

binary_version() { "$1" --version 2>/dev/null | awk '{print $2}'; }
have() { command -v "$1" >/dev/null 2>&1; }

# The running processes, one per line, each line starting with the path the process was started ~keep
# from. Fails when the list cannot be read. On Unix a line is the whole argument string. The ps of ~keep
# Git Bash, MSYS2 and Cygwin takes none of the POSIX output options; its -W lists every Windows ~keep
# process with the executable's path as the last column, which is cut out from the first drive ~keep
# letter or slash on, since the columns before it hold neither. ~keep
process_list() {
  have ps || return 1
  if [ -n "$MSYS_SHELL" ]; then
    ps -W 2>/dev/null | awk 'NR > 1 && match($0, /[A-Za-z]:[\\\/]|\//) { print substr($0, RSTART) }'
  else
    # -ww: BSD and macOS ps otherwise cut each line at the terminal width. ~keep
    ps -A -ww -o args= 2>/dev/null
  fi
}

# Whether a running process may have been started from the cached version directory $1 (trailing ~keep
# slash included). Each line of the process list is matched by prefix, so a cache path with spaces ~keep
# still matches, against the directory as given and as `pwd -P` resolves it. Under an MSYS shell ~keep
# the list holds Windows paths, so both spellings go through cygpath first and the comparison ~keep
# ignores case and the direction of the slashes. Answers yes when the process list cannot be read ~keep
# or a path cannot be converted: an unused directory costs disk, a deleted one in use costs a ~keep
# session. ~keep
version_in_use() {
  local dir="$1" physical listing
  listing="$(process_list)" || return 0
  [ -n "$listing" ] || return 0
  physical="$(cd "$dir" 2>/dev/null && pwd -P)" || physical=""
  [ -n "$physical" ] && physical="${physical%/}/"
  if [ -n "$MSYS_SHELL" ]; then
    have cygpath || return 0
    dir="$(cygpath -m "$dir" 2>/dev/null)" || return 0
    [ -n "$dir" ] || return 0
    dir="${dir%/}/"
    if [ -n "$physical" ]; then
      physical="$(cygpath -m "$physical" 2>/dev/null)" || return 0
      [ -n "$physical" ] && physical="${physical%/}/"
    fi
  fi
  # Through the environment, not -v: awk unescapes a -v value, and a path may hold a backslash. ~keep
  printf '%s\n' "$listing" | PRUNE_DIR="$dir" PRUNE_PHYSICAL="$physical" PRUNE_WINDOWS="$MSYS_SHELL" awk '
    BEGIN {
      dir = ENVIRON["PRUNE_DIR"]
      physical = ENVIRON["PRUNE_PHYSICAL"]
      windows = ENVIRON["PRUNE_WINDOWS"]
      if (windows != "") {
        dir = tolower(dir)
        physical = tolower(physical)
      }
    }
    {
      line = $0
      if (windows != "") {
        gsub(/\\/, "/", line)
        line = tolower(line)
      }
      if (index(line, dir) == 1 || (physical != "" && index(line, physical) == 1)) found = 1
    }
    END { exit !found }'
}

# Whether $1 was last modified more than $2 minutes ago. No when its age cannot be read. ~keep
older_than_minutes() {
  local old
  old="$(find "$1" -maxdepth 0 -mmin "+$2" 2>/dev/null)" || return 1
  [ -n "$old" ]
}

# Record that the cached version in directory $1 is being launched now. A launcher is a shell until ~keep
# it execs, so the process list cannot show that it is about to start that version; the marker can. ~keep
mark_used() {
  [ -d "$1" ] || return 0
  touch "$1/.last-used" 2>/dev/null || true
}

# Whether the cached version directory $1 (trailing slash included) was last launched more than ~keep
# PRUNE_AFTER_DAYS ago, by its marker, or by its own age when it has none. ~keep
unused_long_enough() {
  local marker="$1.last-used"
  [ -e "$marker" ] || marker="$1"
  older_than_minutes "$marker" "$((PRUNE_AFTER_DAYS * 1440))"
}

# Remove the cached version directories under $1, except the one named $2, but only those nothing ~keep
# has launched for PRUNE_AFTER_DAYS and no running process was started from (another install, or a ~keep
# session opened before an update, whose daemon re-executes the binary), and never one an install ~keep
# holds the lock for. Nothing here stops a process of another version; daemons of different ~keep
# versions coexist, and each exits on its own once idle. A symlinked entry is never touched: the ~keep
# launcher did not create it, and removing through it would empty the directory it points at. ~keep
prune_unused_under() {
  local parent="$1" keep="$2" entry base
  [ -d "$parent" ] || return 0
  for entry in "$parent"/*/; do
    [ -d "$entry" ] || continue
    [ -L "${entry%/}" ] && continue
    base="$(basename "$entry")"
    [ "$base" = "$keep" ] && continue
    case "$base" in
    [0-9]*)
      [ -e "$parent/.lock-$base" ] && continue
      unused_long_enough "$entry" || continue
      version_in_use "$entry" || rm -rf "${entry%/}" 2>/dev/null || true
      ;;
    esac
  done
}

# Prune this launcher's cache, and the cache the launchers up to 0.33 left behind. A version there ~keep
# has no marker, so it is judged by its directory's age: one installed long ago that nothing runs ~keep
# from. If a 0.33 launcher is still in use after all, it downloads its version again. ~keep
prune_stale_versions() {
  prune_unused_under "$PARENT" "$VERSION"
  prune_unused_under "$LEGACY_PARENT" ""
}

# Remove install lock and staging directories a crashed install left behind. Another version's ~keep
# install may be running right now, so an entry goes only once it has sat untouched for ~keep
# STALE_INSTALL_MINUTES, and a staging directory only when the process named in it is gone too. ~keep
# The lock for VERSION is never removed here: its waiters break it themselves. ~keep
cleanup_stale_state() {
  [ -d "$PARENT" ] || return 0
  local entry pid
  for entry in "$PARENT"/.lock-* "$PARENT"/.staging-*; do
    [ -e "$entry" ] || continue
    [ "$entry" = "$PARENT/.lock-$VERSION" ] && continue
    older_than_minutes "$entry" "$STALE_INSTALL_MINUTES" || continue
    case "${entry##*/}" in
    .staging-*)
      pid="${entry##*-}"
      case "$pid" in
      '' | *[!0-9]*) ;;
      *) kill -0 "$pid" 2>/dev/null && continue ;;
      esac
      ;;
    esac
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
  mark_used "$CACHE_ROOT"
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
    die "macOS on Intel (x86_64) is not supported; basemind publishes Apple Silicon (arm64) binaries only"
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
    mark_used "$(dirname "$fb")"
    cleanup_stale_state
    exec "$fb" "${ARGS[@]}"
  fi
  die_incomplete_release "$reason"
}

LOCK_HELD=""
waited=0
while ! mkdir "$LOCK" 2>/dev/null; do
  mark_used "$CACHE_ROOT"
  try_exec "$MANAGED_BIN" "$@"
  sleep 0.2
  waited=$((waited + 1))
  if [ "$waited" -ge 600 ]; then
    rmdir "$LOCK" 2>/dev/null || true
    waited=0
  fi
done
LOCK_HELD=1

mark_used "$CACHE_ROOT"
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
mark_used "$CACHE_ROOT"
log "installed basemind $VERSION to $CACHE_ROOT"

rm -rf "$TMP"
TMP=""
release_lock
LOCK_HELD=""

prune_stale_versions
cleanup_stale_state

exec "$MANAGED_BIN" "$@"
