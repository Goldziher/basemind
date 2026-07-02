#!/usr/bin/env bash
# basemind agent-comms background monitor (Claude Code plugin `monitors/`).
#
# Runs for the lifetime of the session and polls this agent's inbox every
# ${BASEMIND_COMMS_POLL_SECS:-15} seconds. Each NEW message (one the agent has not seen yet, and
# not authored itself — the broker excludes self-authored messages from the inbox) is emitted as
# a single stdout line, which the Monitor mechanism delivers to the model as one notification.
#
# Unlike the per-turn `inbox-notify` hook, this fires WITHOUT user input, so an agent picks up
# room traffic while it is working or idle. Design points:
#   * Front-matter only — never prints message bodies (the model calls message_get on demand).
#   * Baseline on first poll — the SessionStart hook already showed recent history, so we record
#     the current high-water timestamp and only announce messages that arrive AFTER startup.
#   * In-memory high-water mark — no temp files; the process is long-lived.
#   * Fail-open and resilient — any poll error (daemon down, no jq, comms feature absent) is
#     swallowed and the loop simply retries on the next tick. The monitor never exits on error.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
LAUNCH="${SCRIPT_DIR}/mcp-launch.sh"

# jq is required to parse the inbox JSON; without it there is nothing this monitor can do.
command -v jq >/dev/null 2>&1 || exit 0
[ -x "$LAUNCH" ] || exit 0

# Poll cadence (seconds), clamped to a sane floor so a misconfig cannot busy-loop.
INTERVAL="${BASEMIND_COMMS_POLL_SECS:-15}"
case "$INTERVAL" in
'' | *[!0-9]*) INTERVAL=15 ;;
esac
[ "$INTERVAL" -ge 5 ] 2>/dev/null || INTERVAL=5

# High-water timestamp (micros). -1 marks "not yet baselined" so the first successful poll
# records the current max without announcing pre-existing messages.
hwm=-1

poll_once() {
	local json maxts new
	json="$(timeout 8 "$LAUNCH" comms inbox --root "$PWD" --json --limit 30 2>/dev/null)" || return 0
	[ -n "$json" ] || return 0

	maxts="$(printf '%s' "$json" | jq -r '[.messages[].ts_micros] | max // 0' 2>/dev/null | tr -cd '0-9-')"
	[ -n "$maxts" ] || maxts=0

	# First successful poll: baseline only, do not replay history.
	if [ "$hwm" -lt 0 ]; then
		hwm="$maxts"
		return 0
	fi

	new="$(printf '%s' "$json" |
		jq -r --argjson hwm "$hwm" \
			'.messages[] | select(.ts_micros > $hwm) | "basemind comms — new message [\(.subject)] from \(.from) (id: \(.id)); call message_get to read the body"' \
			2>/dev/null)"

	if [ -n "$new" ]; then
		printf '%s\n' "$new"
		hwm="$maxts"
	fi
}

while :; do
	poll_once
	sleep "$INTERVAL"
done
