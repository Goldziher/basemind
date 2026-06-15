#!/usr/bin/env bash
# basemind statusline — one-line live summary of the indexed code map.
#
# Wire it into Claude Code by adding to ~/.claude/settings.json (the
# plugin manifest does not yet support `statusLine`, so this step is
# manual until Claude Code adds the field to `.claude-plugin/plugin.json`):
#
#   {
#     "statusLine": {
#       "type": "command",
#       "command": "$HOME/.claude/plugins/basemind/.claude-plugin/statusline.sh",
#       "refreshInterval": 5
#     }
#   }
#
# Claude Code feeds the script a JSON payload on stdin; we extract
# `workspace.current_dir` (or fall back to PWD) and look for `.basemind/` under
# it. Missing `.basemind/` → silent empty line so the script never breaks repos
# that don't use basemind.
#
# All filesystem reads are bounded (`tail -n 1000`) so the script stays cheap
# even at `refreshInterval: 1`. `jq` is required.

set -euo pipefail

# ─── Workspace ─────────────────────────────────────────────────────────────────
input="$(cat 2>/dev/null || true)"
cwd=""
if [[ -n "$input" ]] && command -v jq >/dev/null 2>&1; then
  cwd="$(printf '%s' "$input" | jq -r '.workspace.current_dir // .cwd // empty' 2>/dev/null || true)"
fi
[[ -z "$cwd" ]] && cwd="${PWD}"

bm_dir="${cwd}/.basemind"
if [[ ! -d "$bm_dir" ]]; then
  # No basemind index — print nothing; the statusline line collapses.
  exit 0
fi

# ─── File count from on-disk manifest ──────────────────────────────────────────
file_count="?"
view_index="${bm_dir}/views/working/index.msgpack"
if [[ -f "$view_index" ]]; then
  # Crude byte→file estimate (~130 bytes per FileEntry). Underestimates large
  # repos slightly; the `~` prefix signals approximation. Precise count is one
  # MCP `status` call away.
  if size="$(wc -c <"$view_index" 2>/dev/null)"; then
    file_count="$((size / 130))"
    [[ "$file_count" -lt 1 ]] && file_count=1
  fi
fi

# ─── Scan recency from index mtime ─────────────────────────────────────────────
scan_age="never"
scan_delta=999999999
if [[ -f "$view_index" ]]; then
  if mtime="$(stat -f %m "$view_index" 2>/dev/null || stat -c %Y "$view_index" 2>/dev/null)"; then
    now="$(date +%s)"
    scan_delta=$((now - mtime))
    if [[ $scan_delta -lt 60 ]]; then
      scan_age="${scan_delta}s ago"
    elif [[ $scan_delta -lt 3600 ]]; then
      scan_age="$((scan_delta / 60))m ago"
    elif [[ $scan_delta -lt 86400 ]]; then
      scan_age="$((scan_delta / 3600))h ago"
    else
      scan_age="$((scan_delta / 86400))d ago"
    fi
  fi
fi

# ─── Telemetry: calls + tokens saved today ─────────────────────────────────────
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

# Compact count formatter — 1234 → 1.2k, 14200 → 14k, 1500000 → 1M.
# Decimal arithmetic done via `n*10/1000 %10` so 1099 → 1.0k (not 1.k).
fmt_count() {
  local n="$1"
  if [[ "$n" -lt 1000 ]]; then
    printf '%d' "$n"
  elif [[ "$n" -lt 10000 ]]; then
    printf '%d.%dk' "$((n / 1000))" "$(((n * 10 / 1000) % 10))"
  elif [[ "$n" -lt 1000000 ]]; then
    printf '%dk' "$((n / 1000))"
  else
    printf '%dM' "$((n / 1000000))"
  fi
}

saved_fmt="$(fmt_count "$saved")"
calls_fmt="$(fmt_count "$calls")"

# ─── Styling ───────────────────────────────────────────────────────────────────
# True-color (24-bit) brand mark, exact match for plugin brandColor #F97316
# (R=249, G=115, B=22). True-color is supported by every modern terminal
# emulator (iTerm2, kitty, alacritty, modern xterm/gnome-terminal, Ghostty,
# Warp, the Claude Code statusline renderer). Terminals that don't support it
# fall back to the closest available palette colour without breaking the line.
brand=$'\033[38;2;249;115;22m'
bold=$'\033[1m'
dim=$'\033[2m'
reset=$'\033[0m'

# Freshness dot — green < 1h, yellow 1–24h, red > 1d. Suppressed entirely when
# the index doesn't exist yet (scan_age="never"); the trailing space collapses
# so the layout stays clean.
dot=""
if [[ "$scan_age" != "never" ]]; then
  if [[ $scan_delta -lt 3600 ]]; then
    dot_color=$'\033[32m' # green
  elif [[ $scan_delta -lt 86400 ]]; then
    dot_color=$'\033[33m' # yellow
  else
    dot_color=$'\033[31m' # red
  fi
  dot="  ${dot_color}●${reset}"
fi

# ─── Render ────────────────────────────────────────────────────────────────────
# Layout: ▲ basemind  144 files · scanned 2d ago  ●  0 calls · 0 tok saved
printf '%s▲%s %sbasemind%s  %s%s files · scanned %s%s%s  %s calls · %s tok saved' \
  "$brand" "$reset" \
  "$bold" "$reset" \
  "$dim" "$file_count" \
  "$scan_age" "$reset" \
  "$dot" \
  "$calls_fmt" \
  "$saved_fmt"
