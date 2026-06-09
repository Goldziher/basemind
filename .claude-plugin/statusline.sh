#!/usr/bin/env bash
# basemind statusline — one-line live summary of the indexed code map.
#
# Wire it into Claude Code by adding to ~/.claude/settings.json:
#
#   {
#     "statusLine": {
#       "type": "command",
#       "command": "$HOME/.claude/plugins/basemind/.claude-plugin/statusline.sh",
#       "refreshInterval": 5
#     }
#   }
#
# Claude Code feeds the script a JSON payload on stdin; we extract `workspace.current_dir`
# (or fall back to PWD) and look for `.basemind/` under it. Missing `.basemind/` → silent
# empty line so the script never breaks repos that don't use basemind.
#
# All filesystem reads are bounded (`tail -n 1000`) so the script stays cheap even at
# `refreshInterval: 1`. `jq` is required.

set -euo pipefail

# Pull the workspace cwd from Claude Code's stdin payload. Fall back to $PWD when
# the script is run standalone (e.g. manual testing).
input="$(cat 2>/dev/null || true)"
cwd=""
if [[ -n "$input" ]] && command -v jq >/dev/null 2>&1; then
  cwd="$(printf '%s' "$input" | jq -r '.workspace.current_dir // .cwd // empty' 2>/dev/null || true)"
fi
if [[ -z "$cwd" ]]; then
  cwd="${PWD}"
fi

bm_dir="${cwd}/.basemind"
if [[ ! -d "$bm_dir" ]]; then
  # No basemind in this repo — print nothing and exit clean. The statusline collapses.
  exit 0
fi

# File count: pull from the on-disk index manifest. The manifest exposes a `files`
# array; counting entries is the cheapest signal. Fall back to "?" when the manifest
# isn't there yet (pre-first-scan).
file_count="?"
view_index="${bm_dir}/views/working/index.msgpack"
if [[ -f "$view_index" ]]; then
  # Crude but cheap: count msgpack "fixstr"/"str8/16/32" path keys via grep. Real
  # parsing would need a msgpack tool; we don't take that dependency for a
  # statusline. For the precise number, the agent can call `status` via MCP.
  if file_count_raw="$(wc -c <"$view_index" 2>/dev/null)"; then
    # Rough estimate: assume 130 bytes per FileEntry. Underestimates large repos
    # slightly but the prefix is `~` so users know it's approximate.
    file_count="$((file_count_raw / 130))"
    [[ "$file_count" -lt 1 ]] && file_count=1
  fi
fi

# Scan recency from the index mtime.
scan_age="never"
if [[ -f "$view_index" ]]; then
  if mtime="$(stat -f %m "$view_index" 2>/dev/null || stat -c %Y "$view_index" 2>/dev/null)"; then
    now="$(date +%s)"
    delta=$((now - mtime))
    if [[ $delta -lt 60 ]]; then
      scan_age="${delta}s ago"
    elif [[ $delta -lt 3600 ]]; then
      scan_age="$((delta / 60))m ago"
    elif [[ $delta -lt 86400 ]]; then
      scan_age="$((delta / 3600))h ago"
    else
      scan_age="$((delta / 86400))d ago"
    fi
  fi
fi

# Telemetry aggregates: total calls + estimated tokens saved today. Bounded read.
tel_file="${bm_dir}/telemetry.jsonl"
calls=0
saved=0
if [[ -f "$tel_file" ]] && command -v jq >/dev/null 2>&1; then
  midnight_us="$(date -v0H -v0M -v0S +%s 2>/dev/null || date -d 'today 00:00' +%s 2>/dev/null || echo 0)"
  midnight_us=$((midnight_us * 1000000))
  read -r calls saved <<<"$(tail -n 1000 "$tel_file" 2>/dev/null |
    jq -rs --arg cutoff "$midnight_us" '
        [.[] | select(.ts_micros >= ($cutoff | tonumber))]
        | "\(length) \([.[].est_tokens_saved] | add // 0)"
      ' 2>/dev/null || echo "0 0")"
  [[ -z "$calls" ]] && calls=0
  [[ -z "$saved" ]] && saved=0
fi

# Compact token-saved formatter (1234 → 1.2k, 14200 → 14k).
fmt_count() {
  local n="$1"
  if [[ "$n" -lt 1000 ]]; then
    printf '%d' "$n"
  elif [[ "$n" -lt 10000 ]]; then
    printf '%d.%dk' "$((n / 1000))" "$(((n % 1000) / 100))"
  elif [[ "$n" -lt 1000000 ]]; then
    printf '%dk' "$((n / 1000))"
  else
    printf '%dM' "$((n / 1000000))"
  fi
}

saved_fmt="$(fmt_count "$saved")"

# ANSI dim color for the prefix so the line doesn't shout.
dim=$'\033[2m'
reset=$'\033[0m'
printf '%sbm%s %s~%sf · scan %s · %s calls · ~%s tok saved' \
  "$dim" "$reset" \
  "$dim" "$file_count" \
  "$scan_age" \
  "$calls" \
  "$saved_fmt"
