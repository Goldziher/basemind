#!/usr/bin/env bash
# basemind statusline — one-line live summary of the indexed code map.
#
# Wire it into Claude Code MANUALLY by adding to ~/.claude/settings.json
# (per the Claude Code plugin schema documented at
# https://code.claude.com/docs/en/plugins-reference — verified 2026-06-17,
# the `plugin.json` manifest does NOT expose a `statusLine` field; the only
# statusline-adjacent key supported there is `subagentStatusLine`, which is
# not what we want here):
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
# `workspace.current_dir` (or fall back to PWD) and look for `.basemind/`
# under it. Missing `.basemind/` is rendered as an actionable hint rather
# than a silent line, so the user knows to run `basemind scan`.
#
# All filesystem reads are bounded (`tail -n 1000`) so the script stays cheap
# even at `refreshInterval: 1`. `jq` is required for telemetry parsing; the
# script degrades gracefully when absent.

set -euo pipefail

# ─── Workspace ─────────────────────────────────────────────────────────────────
input="$(cat 2>/dev/null || true)"
cwd=""
if [[ -n "$input" ]] && command -v jq >/dev/null 2>&1; then
  cwd="$(printf '%s' "$input" | jq -r '.workspace.current_dir // .cwd // empty' 2>/dev/null || true)"
fi
[[ -z "$cwd" ]] && cwd="${PWD}"

bm_dir="${cwd}/.basemind"

# ─── Styling ───────────────────────────────────────────────────────────────────
# True-color (24-bit) brand mark — exact #F97316. Every other colour is from
# the bright 256-colour palette so terminals without true-color still render
# legibly. Nothing is dim — readability is the whole point of this redesign.
brand=$'\033[38;2;249;115;22m' # brand orange (◆ + "basemind")
cyan=$'\033[38;5;51m'          # bright cyan — file count, scan-age, intel
magenta=$'\033[38;5;201m'      # bright magenta — calls, tokens saved
label=$'\033[38;5;255m'        # soft white — units / labels
sep=$'\033[38;5;240m'          # light grey — section separators
bold=$'\033[1m'
reset=$'\033[0m'

glyph="◆"

# ─── Empty / missing cases ─────────────────────────────────────────────────────
# `.basemind/` missing entirely → actionable hint.
if [[ ! -d "$bm_dir" ]]; then
  printf '%s%s%s %s%sbasemind%s %s│%s %sno index — run:%s %s%sbasemind scan%s' \
    "$brand" "$glyph" "$reset" \
    "$bold" "$brand" "$reset" \
    "$sep" "$reset" \
    "$label" "$reset" \
    "$bold" "$cyan" "$reset"
  exit 0
fi

# `.basemind/` exists but no blobs yet (scan in progress / freshly init'd).
blobs_dir="${bm_dir}/blobs"
if [[ ! -d "$blobs_dir" ]] || [[ -z "$(find "$blobs_dir" -maxdepth 1 -type f -name '*.l1.msgpack' -print -quit 2>/dev/null)" ]]; then
  printf '%s%s%s %s%sbasemind%s %s│%s %sscanning…%s' \
    "$brand" "$glyph" "$reset" \
    "$bold" "$brand" "$reset" \
    "$sep" "$reset" \
    "$label" "$reset"
  exit 0
fi

# ─── File count — count *.l1.msgpack in blobs/ (one per unique file content) ──
file_count=0
file_count_raw="$(find "$blobs_dir" -maxdepth 1 -type f -name '*.l1.msgpack' 2>/dev/null | wc -l || echo 0)"
# Trim leading whitespace from `wc -l`.
file_count="${file_count_raw##*[[:space:]]}"
[[ -z "$file_count" ]] && file_count=0

# ─── Scan recency — most-recent mtime of index.msgpack or index.fjall/ ────────
scan_age="never"
scan_delta=999999999
scan_mtime=0
view_index="${bm_dir}/views/working/index.msgpack"
view_fjall="${bm_dir}/views/working/index.fjall"
for candidate in "$view_index" "$view_fjall"; do
  if [[ -e "$candidate" ]]; then
    m="$(stat -f %m "$candidate" 2>/dev/null || stat -c %Y "$candidate" 2>/dev/null || echo 0)"
    if [[ "$m" -gt "$scan_mtime" ]]; then
      scan_mtime="$m"
    fi
  fi
done
if [[ "$scan_mtime" -gt 0 ]]; then
  now="$(date +%s)"
  scan_delta=$((now - scan_mtime))
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

# ─── Telemetry: calls + tokens saved today ─────────────────────────────────────
tel_file="${bm_dir}/telemetry.jsonl"
calls=0
saved=0
tel_mtime=0
if [[ -f "$tel_file" ]]; then
  tel_mtime="$(stat -f %m "$tel_file" 2>/dev/null || stat -c %Y "$tel_file" 2>/dev/null || echo 0)"
  if command -v jq >/dev/null 2>&1; then
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
fi

# ─── Intelligence sidecar (documents / memory / web) ──────────────────────────
# Presence-only probe — row counts would require LanceDB reads too slow for a
# 5-second refresh interval. The intelligence row is suppressed when the
# top-level lance/ directory doesn't exist (typical for code-map-only repos).
lance_dir="${bm_dir}/lance"
have_intel=0
docs_present=0
mem_present=0
web_present=0
if [[ -d "$lance_dir" ]]; then
  [[ -d "${lance_dir}/documents.lance" ]] && docs_present=1
  [[ -d "${lance_dir}/memory.lance" ]] && mem_present=1
  [[ -d "${lance_dir}/web.lance" ]] && web_present=1
  if [[ $docs_present -eq 1 ]] || [[ $mem_present -eq 1 ]] || [[ $web_present -eq 1 ]]; then
    have_intel=1
  fi
  # If a sidecar dropped exact counts, prefer those.
  intel_sidecar="${bm_dir}/views/working/.intelligence_count.json"
  if [[ -f "$intel_sidecar" ]] && command -v jq >/dev/null 2>&1; then
    have_intel=1
    docs_present="$(jq -r '.documents // 0' "$intel_sidecar" 2>/dev/null || echo 0)"
    mem_present="$(jq -r '.memory // 0' "$intel_sidecar" 2>/dev/null || echo 0)"
    web_present="$(jq -r '.web // 0' "$intel_sidecar" 2>/dev/null || echo 0)"
  fi
fi

# ─── Liveness state dot ───────────────────────────────────────────────────────
# green  ● — serve fresh (telemetry mtime <60s) OR scan <1h with no telemetry
# amber  ● — serve idle (pgrep hit but stale telemetry) OR scan 1–24h
# red    ● — no serve AND scan >24h
now="$(date +%s)"
tel_age=$((now - tel_mtime))
serve_running=0
if command -v pgrep >/dev/null 2>&1; then
  if pgrep -f "basemind serve" >/dev/null 2>&1; then
    serve_running=1
  fi
fi
if [[ "$tel_mtime" -gt 0 ]] && [[ "$tel_age" -lt 60 ]]; then
  dot_color=$'\033[38;5;46m' # bright green
elif [[ $scan_delta -lt 3600 ]] && [[ "$calls" -eq 0 ]]; then
  dot_color=$'\033[38;5;46m'
elif [[ $serve_running -eq 1 ]] || { [[ $scan_delta -ge 3600 ]] && [[ $scan_delta -lt 86400 ]]; }; then
  dot_color=$'\033[38;5;214m' # amber
else
  dot_color=$'\033[38;5;196m' # red
fi
dot="${dot_color}●${reset}"

# ─── Formatters ───────────────────────────────────────────────────────────────
# Compact count formatter — 1234 → 1.2k, 14200 → 14k, 1500000 → 1M.
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

# Thousands-separator formatter for the wide layout. POSIX `printf "%'d"`
# only honours the apostrophe flag under a locale that defines a numeric
# grouping; force one inline. Falls back to bare digits on broken locales.
fmt_thousands() {
  local n="$1"
  LC_ALL=en_US.UTF-8 printf "%'d" "$n" 2>/dev/null || printf '%d' "$n"
}

files_wide="$(fmt_thousands "$file_count")"
files_narrow="$(fmt_count "$file_count")"
calls_fmt="$(fmt_count "$calls")"
saved_fmt="$(fmt_count "$saved")"

# ─── Width detection ──────────────────────────────────────────────────────────
cols="$(tput cols 2>/dev/null || echo 120)"
[[ -z "$cols" ]] && cols=120

# ─── Render ───────────────────────────────────────────────────────────────────
if [[ "$cols" -ge 100 ]]; then
  # Wide layout. Render as concatenated coloured segments.
  out=""
  out+="${brand}${glyph}${reset} "
  out+="${bold}${brand}basemind${reset}  "
  out+="${dot}  "
  out+="${bold}${cyan}${files_wide}${reset} ${label}files${reset} "
  out+="${sep}·${reset} "
  out+="${bold}${cyan}${scan_age}${reset}"
  out+="  ${sep}│${reset}  "
  out+="${bold}${magenta}${calls_fmt}${reset} ${label}calls${reset} "
  out+="${sep}·${reset} "
  out+="${bold}${magenta}${saved_fmt}${reset} ${label}saved${reset}"
  if [[ $have_intel -eq 1 ]]; then
    out+="  ${sep}│${reset}  "
    if [[ "$docs_present" != 0 ]]; then
      out+="${bold}${cyan}${docs_present}${reset} ${label}docs${reset}"
    fi
    if [[ "$mem_present" != 0 ]]; then
      [[ "$docs_present" != 0 ]] && out+=" ${sep}·${reset} "
      out+="${bold}${cyan}${mem_present}${reset} ${label}mem${reset}"
    fi
    if [[ "$web_present" != 0 ]]; then
      { [[ "$docs_present" != 0 ]] || [[ "$mem_present" != 0 ]]; } && out+=" ${sep}·${reset} "
      out+="${bold}${cyan}${web_present}${reset} ${label}sites${reset}"
    fi
  fi
  printf '%s' "$out"
else
  # Narrow layout.
  out=""
  out+="${brand}${glyph}${reset} "
  out+="${bold}${brand}basemind${reset} "
  out+="${dot} "
  out+="${bold}${cyan}${files_narrow}${reset} ${sep}·${reset} "
  out+="${bold}${cyan}${scan_age% ago}${reset} "
  out+="${sep}│${reset} "
  out+="${bold}${magenta}${calls_fmt}${reset}${label}c${reset} ${sep}·${reset} "
  out+="${bold}${magenta}${saved_fmt}${reset} ${label}saved${reset}"
  printf '%s' "$out"
fi
