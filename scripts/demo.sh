#!/usr/bin/env bash
# basemind CLI demo — a deterministic, narrated command sequence for recording.
#
# This is the script `asciinema rec` drives (via scripts/demo-record.sh) to
# produce docs/media/demo.gif for the README. It scans an isolated clone of
# basemind's OWN repo and exercises the query / git / telemetry surface, so the
# recording shows real, dense output and ends on a populated token-savings
# dashboard (the CLI query calls flow through the same tool path that records
# telemetry).
#
# Why a clone, not the repo in place: a running `basemind serve` (the editor
# plugin) holds .basemind/.lock, which would make `basemind scan` fail; a fresh
# clone also gives a clean, reproducible telemetry sink. The clone is removed on
# exit.
#
# Run it directly to preview (no recording):  ./scripts/demo.sh
# Override the binary for a dev build:         BASEMIND_BIN=target/release/basemind ./scripts/demo.sh
# Scale the pacing (1.0 = default):            DEMO_SPEED=0.5 ./scripts/demo.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Force color on even though stdout may not look like a TTY to the binary under
# the recorder; clear any inherited NO_COLOR that would suppress it; silence the
# INFO tracing logs so only command output shows.
export CLICOLOR_FORCE=1
export RUST_LOG="${RUST_LOG:-error}"
unset NO_COLOR 2>/dev/null || true

# Pacing knobs (seconds). DEMO_SPEED scales them: <1 faster, >1 slower.
SPEED="${DEMO_SPEED:-1.0}"
TYPE_DELAY="$(awk -v s="$SPEED" 'BEGIN { printf "%.4f", 0.03 * s }')"  # per keystroke
PROMPT_PAUSE="$(awk -v s="$SPEED" 'BEGIN { printf "%.4f", 0.6 * s }')" # after output
PROMPT="$(printf '\033[1;32m❯\033[0m ')"                               # green prompt

# Resolve the basemind binary to an ABSOLUTE path (we cd into a temp clone
# below): explicit override, then release build, then PATH.
resolve_bin() {
  if [ -n "${BASEMIND_BIN:-}" ] && [ -x "${BASEMIND_BIN}" ]; then
    (cd "$(dirname "$BASEMIND_BIN")" && printf '%s/%s' "$PWD" "$(basename "$BASEMIND_BIN")")
  elif [ -x "$REPO_ROOT/target/release/basemind" ]; then
    printf '%s' "$REPO_ROOT/target/release/basemind"
  elif command -v basemind >/dev/null 2>&1; then
    command -v basemind
  else
    printf 'demo: no basemind binary found — run: cargo build --release (or set BASEMIND_BIN)\n' >&2
    exit 1
  fi
}
BIN="$(resolve_bin)"

# Isolated clone so `scan` never contends with a running server and telemetry
# starts clean. Full history comes along, so the git tools work.
WORKDIR="$(mktemp -d)"
cleanup() { rm -rf "$WORKDIR" 2>/dev/null || true; }
trap cleanup EXIT
git clone -q "$REPO_ROOT" "$WORKDIR/basemind"
cd "$WORKDIR/basemind"

# pe = print + execute. "Types" the command after a prompt, runs it, then pauses
# so the viewer can read the output before the next command.
pe() {
  printf '%s' "$PROMPT"
  local i ch
  for ((i = 0; i < ${#1}; i++)); do
    ch="${1:$i:1}"
    printf '%s' "$ch"
    sleep "$TYPE_DELAY"
  done
  printf '\n'
  # Run via the resolved binary; the displayed line uses the friendly `basemind`.
  local cmd="${1/#basemind/$BIN}"
  eval "$cmd" || true
  printf '\n'
  sleep "$PROMPT_PAUSE"
}

# --- the narrated sequence ----------------------------------------------------
# 1. Build the code map for this repo (--quiet: just the summary line, no
#    per-file warnings, for a clean opening frame).
pe "basemind scan --quiet"
# 2. Real file structure with calls + docs.
pe "basemind query outline src/scanner.rs --l2"
# 3. Substring symbol search.
pe "basemind query search scan --limit 10"
# 4. Call sites of a symbol (dense).
pe "basemind query references record_call --limit 8"
# 5. Transitive callers graph.
pe "basemind query call-graph cmd_scan --direction callers --max-depth 3"
# 6. Git history surface.
pe "basemind git recent-changes --limit 5"
# 7. Blame clamped to a symbol.
pe "basemind git blame-symbol src/main.rs cmd_scan"
# 8. The payoff: a populated token-savings dashboard from the calls above.
pe "basemind telemetry --window today"
