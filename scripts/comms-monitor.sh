#!/usr/bin/env bash
# not authored itself — the broker excludes self-authored messages from the inbox) is emitted as

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
LAUNCH="${SCRIPT_DIR}/mcp-launch.sh"

command -v jq >/dev/null 2>&1 || exit 0
[ -x "$LAUNCH" ] || exit 0

INTERVAL="${BASEMIND_COMMS_POLL_SECS:-15}"
case "$INTERVAL" in
'' | *[!0-9]*) INTERVAL=15 ;;
esac
[ "$INTERVAL" -ge 5 ] 2>/dev/null || INTERVAL=5

hwm=-1

poll_once() {
	local json maxts new
	json="$(timeout 8 "$LAUNCH" comms inbox --root "$PWD" --json --limit 30 2>/dev/null)" || return 0
	[ -n "$json" ] || return 0

	maxts="$(printf '%s' "$json" | jq -r '[.messages[].ts_micros] | max // 0' 2>/dev/null | tr -cd '0-9-')"
	[ -n "$maxts" ] || maxts=0

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
