#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

export CLICOLOR_FORCE=1
export RUST_LOG="${RUST_LOG:-error}"
unset NO_COLOR 2>/dev/null || true

SPEED="${DEMO_SPEED:-1.0}"
TYPE_DELAY="$(awk -v s="$SPEED" 'BEGIN { printf "%.4f", 0.03 * s }')"
PROMPT_PAUSE="$(awk -v s="$SPEED" 'BEGIN { printf "%.4f", 0.6 * s }')"
PROMPT="$(printf '\033[1;32m❯\033[0m ')"

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

WORKDIR="$(mktemp -d)"
cleanup() { rm -rf "$WORKDIR" 2>/dev/null || true; }
trap cleanup EXIT
git clone -q "$REPO_ROOT" "$WORKDIR/basemind"
cd "$WORKDIR/basemind"

pe() {
	printf '%s' "$PROMPT"
	local i ch
	for ((i = 0; i < ${#1}; i++)); do
		ch="${1:$i:1}"
		printf '%s' "$ch"
		sleep "$TYPE_DELAY"
	done
	printf '\n'
	local cmd="${1/#basemind/$BIN}"
	eval "$cmd" || true
	printf '\n'
	sleep "$PROMPT_PAUSE"
}

pe "basemind scan --quiet"
pe "basemind query outline src/scanner.rs --l2"
pe "basemind query search scan --limit 10"
pe "basemind query references record_call --limit 8"
pe "basemind query call-graph cmd_scan --direction callers --max-depth 3"
pe "basemind git recent-changes --limit 5"
pe "basemind git blame-symbol src/main.rs cmd_scan"
pe "basemind telemetry --window today"
