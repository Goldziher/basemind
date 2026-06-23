#!/usr/bin/env bash
# basemind statusline — live, responsive, two-line summary.
#
# Line 1 (context): model · output-style · dir · branch · context% — reconstructs
#   the "Claude" context Claude Code's default status line would show, because a
#   custom statusLine REPLACES the default (it cannot render below it). Toggle off
#   with BASEMIND_STATUSLINE_CONTEXT=0 for a single basemind line.
# Line 2 (basemind): index health + per-capability activity (calls, searches, git,
#   docs, memory, web) + estimated tokens saved.
#
# Wire it into Claude Code by running `/bm-statusline` once, or manually in
# ~/.claude/settings.json:
#
#   { "statusLine": { "type": "command",
#       "command": "$HOME/.claude/plugins/basemind/.claude-plugin/statusline.sh",
#       "refreshInterval": 5 } }
#
# Layout adapts to terminal width via $COLUMNS (Claude Code sets it; needs CC
# v2.1.153+). Force a tier with BASEMIND_STATUSLINE=full|compact|minimal (default
# auto). All filesystem reads are bounded so the script stays cheap at
# refreshInterval: 1. `jq` is required for telemetry/context parsing; the script
# degrades gracefully when absent.

set -euo pipefail

# ─── Input ─────────────────────────────────────────────────────────────────────
input="$(cat 2>/dev/null || true)"
have_jq=0
command -v jq >/dev/null 2>&1 && have_jq=1

json() { # json <jq-filter> <default>
  local out=""
  if [[ $have_jq -eq 1 && -n "$input" ]]; then
    out="$(printf '%s' "$input" | jq -r "$1 // empty" 2>/dev/null || true)"
  fi
  [[ -n "$out" ]] && printf '%s' "$out" || printf '%s' "$2"
}

cwd="$(json '.workspace.current_dir // .cwd' "${PWD}")"
model="$(json '.model.display_name' '')"
out_style="$(json '.output_style.name' '')"
vim_mode="$(json '.vim.mode' '')"
ctx_pct="$(json '.context_window.used_percentage' '')"
bm_dir="${cwd}/.basemind"

# ─── Width / tier ──────────────────────────────────────────────────────────────
cols="${COLUMNS:-0}"
[[ "$cols" -le 0 ]] && cols="$(tput cols 2>/dev/null || echo 120)"
[[ -z "$cols" || "$cols" -le 0 ]] && cols=120

tier="${BASEMIND_STATUSLINE:-auto}"
if [[ "$tier" == "auto" ]]; then
  if [[ "$cols" -ge 120 ]]; then
    tier="full"
  elif [[ "$cols" -ge 80 ]]; then
    tier="compact"
  else
    tier="minimal"
  fi
fi

# ─── Styling ───────────────────────────────────────────────────────────────────
brand=$'\033[38;2;249;115;22m' # brand orange
cyan=$'\033[38;5;51m'          # file count, scan-age, intel
magenta=$'\033[38;5;201m'      # calls, tokens saved
green=$'\033[38;5;46m'         # comms: unread messages
label=$'\033[38;5;255m'        # units / labels
sep=$'\033[38;5;240m'          # separators
muted=$'\033[38;5;244m'        # context line text
bold=$'\033[1m'
reset=$'\033[0m'
glyph="◆"

# ─── Formatters ────────────────────────────────────────────────────────────────
fmt_count() { # 1234 → 1.2k, 14200 → 14k, 1500000 → 1M
  local n="$1"
  if [[ "$n" -lt 1000 ]]; then
    printf '%d' "$n"
  elif [[ "$n" -lt 10000 ]]; then
    printf '%d.%dk' "$((n / 1000))" "$(((n * 10 / 1000) % 10))"
  elif [[ "$n" -lt 1000000 ]]; then
    printf '%dk' "$((n / 1000))"
  else printf '%dM' "$((n / 1000000))"; fi
}
fmt_thousands() {
  local n="$1"
  LC_ALL=en_US.UTF-8 printf "%'d" "$n" 2>/dev/null || printf '%d' "$n"
}

# Agent-comms state — prints "<unread> <agent>" or nothing. Cheap and side-effect-free:
#   * only runs when a comms-capable `basemind` is on PATH (the cargo `--features comms`
#     build) AND the broker daemon is ALREADY running — never auto-spawns it;
#   * TTL-cached (8s) per cwd so a tight refreshInterval hits the daemon at most once
#     per window;
#   * one bounded `comms inbox --limit 1` call yields the total unread count
#     (page length + the `unread` remainder); identity comes from the env or the cheap
#     persisted `.basemind/agent-id` read.
comms_state() {
  command -v basemind >/dev/null 2>&1 || return 1
  command -v pgrep >/dev/null 2>&1 && pgrep -f "comms daemon" >/dev/null 2>&1 || return 1
  [[ $have_jq -eq 1 ]] || return 1

  local cache now cts cu ca json unread agent
  cache="${TMPDIR:-/tmp}/basemind-sl-comms-$(printf '%s' "$cwd" | cksum | cut -d' ' -f1)"
  now="$(date +%s)"
  if [[ -f "$cache" ]]; then
    read -r cts cu ca <"$cache" 2>/dev/null || true
    if [[ -n "${cts:-}" && "$cts" =~ ^[0-9]+$ ]] && ((now - cts < 8)); then
      printf '%s %s' "${cu:-0}" "${ca:-}"
      return 0
    fi
  fi

  json="$(timeout 2 basemind comms inbox --root "$cwd" --json --limit 1 2>/dev/null || true)"
  [[ -n "$json" ]] || return 1
  unread="$(printf '%s' "$json" | jq -r '((.messages | length) + (.unread // 0))' 2>/dev/null | tr -cd '0-9' || true)"
  [[ -n "$unread" ]] || unread=0
  agent="${BASEMIND_AGENT_ID:-}"
  [[ -z "$agent" ]] && agent="$(tr -d '[:space:]' <"${bm_dir}/agent-id" 2>/dev/null || true)"
  printf '%s %s %s\n' "$now" "$unread" "${agent:-}" >"$cache" 2>/dev/null || true
  printf '%s %s' "$unread" "${agent:-}"
}

# ─── Line 1: Claude context ────────────────────────────────────────────────────
build_context_line() {
  [[ "${BASEMIND_STATUSLINE_CONTEXT:-1}" == "0" ]] && return 1
  [[ -z "$model" && -z "$out_style" ]] && return 1 # nothing useful to show

  local dir branch="" head_file parts=()
  dir="$(basename "$cwd")"
  head_file="${cwd}/.git/HEAD"
  if [[ -f "$head_file" ]]; then
    local ref
    ref="$(IFS= read -r ref <"$head_file" && printf '%s' "$ref" || true)"
    [[ "$ref" == ref:\ refs/heads/* ]] && branch="${ref#ref: refs/heads/}"
  fi

  [[ -n "$model" ]] && parts+=("${bold}${muted}${model}${reset}")
  if [[ "$tier" == "full" ]]; then
    [[ -n "$out_style" && "$out_style" != "default" ]] && parts+=("${muted}${out_style}${reset}")
    [[ -n "$vim_mode" ]] && parts+=("${muted}${vim_mode}${reset}")
  fi
  parts+=("${muted}${dir}${reset}")
  [[ -n "$branch" ]] && parts+=("${muted}⎇ ${branch}${reset}")
  if [[ -n "$ctx_pct" && "$tier" != "minimal" ]]; then
    parts+=("${muted}${ctx_pct}% ctx${reset}")
  fi

  local line="" i
  for i in "${!parts[@]}"; do
    [[ "$i" -gt 0 ]] && line+=" ${sep}·${reset} "
    line+="${parts[$i]}"
  done
  printf '%s' "$line"
}

# ─── Line 2: basemind ──────────────────────────────────────────────────────────
mark() { printf '%s%s%s %s%sbasemind%s' "$brand" "$glyph" "$reset" "$bold" "$brand" "$reset"; }

build_basemind_line() {
  # No index → actionable hint.
  if [[ ! -d "$bm_dir" ]]; then
    printf '%s %s│%s %sno index — run:%s %s%sbasemind scan%s' \
      "$(mark)" "$sep" "$reset" "$label" "$reset" "$bold" "$cyan" "$reset"
    return
  fi
  local blobs_dir="${bm_dir}/blobs"
  if [[ ! -d "$blobs_dir" ]] || [[ -z "$(find "$blobs_dir" -maxdepth 1 -type f -name '*.fm.msgpack' -print -quit 2>/dev/null)" ]]; then
    printf '%s %s│%s %sscanning…%s' "$(mark)" "$sep" "$reset" "$label" "$reset"
    return
  fi

  # File count.
  local file_count
  file_count="$(find "$blobs_dir" -maxdepth 1 -type f -name '*.fm.msgpack' 2>/dev/null | wc -l || echo 0)"
  file_count="${file_count##*[[:space:]]}"
  [[ -z "$file_count" ]] && file_count=0

  # Scan recency.
  local scan_age="never" scan_delta=999999999 scan_mtime=0 m now
  for candidate in "${bm_dir}/views/working/index.msgpack" "${bm_dir}/views/working/index.fjall"; do
    if [[ -e "$candidate" ]]; then
      m="$(stat -f %m "$candidate" 2>/dev/null || stat -c %Y "$candidate" 2>/dev/null || echo 0)"
      [[ "$m" -gt "$scan_mtime" ]] && scan_mtime="$m"
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
    else scan_age="$((scan_delta / 86400))d ago"; fi
  fi

  # Telemetry buckets (today): calls, saved, and per-capability counts.
  local calls=0 saved=0 code=0 git=0 docs=0 mem=0 web=0 tel_mtime=0
  local tel_file="${bm_dir}/telemetry.jsonl"
  if [[ -f "$tel_file" ]]; then
    tel_mtime="$(stat -f %m "$tel_file" 2>/dev/null || stat -c %Y "$tel_file" 2>/dev/null || echo 0)"
    if [[ $have_jq -eq 1 ]]; then
      local midnight_us
      midnight_us="$(date -v0H -v0M -v0S +%s 2>/dev/null || date -d 'today 00:00' +%s 2>/dev/null || echo 0)"
      midnight_us=$((midnight_us * 1000000))
      read -r calls saved code git docs mem web <<<"$(tail -n 2000 "$tel_file" 2>/dev/null |
        jq -rs --argjson cut "$midnight_us" '
            def bucket:
              if   (test("^(search_symbols|outline|find_references|find_callers|find_implementations|call_graph|dependents|list_files|workspace_grep|status|repo_info)$")) then "code"
              elif (test("^(blame_|recent_changes|commits_touching|find_commits_by_path|diff_|hot_files|symbol_history|working_tree_status)")) then "git"
              elif (.=="search_documents") then "docs"
              elif (startswith("memory_")) then "mem"
              elif (startswith("web_")) then "web"
              else "code" end;
            map(select(.ts_micros >= $cut))
            | { calls: length, saved: ([.[].est_tokens_saved] | add // 0) } as $tot
            | (map(.tool|bucket) | group_by(.) | map({(.[0]): length}) | add // {}) as $b
            | "\($tot.calls) \($tot.saved) \($b.code // 0) \($b.git // 0) \($b.docs // 0) \($b.mem // 0) \($b.web // 0)"
          ' 2>/dev/null || echo "0 0 0 0 0 0 0")"
      [[ -z "$calls" ]] && calls=0
      [[ -z "$saved" ]] && saved=0
    fi
  fi

  # Liveness dot.
  local now2 tel_age dot_color serve_running=0
  now2="$(date +%s)"
  tel_age=$((now2 - tel_mtime))
  command -v pgrep >/dev/null 2>&1 && pgrep -f "basemind serve" >/dev/null 2>&1 && serve_running=1
  if [[ "$tel_mtime" -gt 0 && "$tel_age" -lt 60 ]]; then
    dot_color=$'\033[38;5;46m'
  elif [[ $scan_delta -lt 3600 && "$calls" -eq 0 ]]; then
    dot_color=$'\033[38;5;46m'
  elif [[ $serve_running -eq 1 || ($scan_delta -ge 3600 && $scan_delta -lt 86400) ]]; then
    dot_color=$'\033[38;5;214m'
  else dot_color=$'\033[38;5;196m'; fi
  local dot="${dot_color}●${reset}"

  # Agent-comms: unread count + identity, only when the broker is live.
  local comms_unread="" comms_agent="" comms_raw
  comms_raw="$(comms_state 2>/dev/null || true)"
  if [[ -n "$comms_raw" ]]; then
    comms_unread="${comms_raw%% *}"
    comms_agent="${comms_raw#* }"
    [[ "$comms_agent" == "$comms_raw" ]] && comms_agent=""
  fi

  # Compose by tier.
  local searches=$((code))
  local out
  out="$(mark)  ${dot}  "
  if [[ "$tier" == "minimal" ]]; then
    out+="${bold}${cyan}$(fmt_count "$file_count")${reset} ${sep}·${reset} "
    out+="${bold}${cyan}${scan_age% ago}${reset} ${sep}│${reset} "
    out+="${bold}${magenta}$(fmt_count "$calls")${reset}${label}c${reset} ${sep}·${reset} "
    out+="${bold}${magenta}$(fmt_count "$saved")${reset} ${label}saved${reset}"
    if [[ -n "$comms_unread" && "$comms_unread" -gt 0 ]]; then
      out+=" ${sep}·${reset} ${bold}${green}✉${comms_unread}${reset}"
    fi
    printf '%s' "$out"
    return
  fi

  local files_disp
  if [[ "$tier" == "full" ]]; then files_disp="$(fmt_thousands "$file_count")"; else files_disp="$(fmt_count "$file_count")"; fi
  out+="${bold}${cyan}${files_disp}${reset} ${label}files${reset} ${sep}·${reset} "
  out+="${bold}${cyan}${scan_age}${reset}"
  out+="  ${sep}│${reset}  "
  out+="${bold}${magenta}$(fmt_count "$calls")${reset} ${label}calls${reset}"

  if [[ "$tier" == "full" ]]; then
    # Per-capability breakdown — only buckets with activity.
    local seg=""
    [[ "$searches" -gt 0 ]] && seg+=" ${sep}·${reset} ${bold}${magenta}$(fmt_count "$searches")${reset} ${label}srch${reset}"
    [[ "$git" -gt 0 ]] && seg+=" ${sep}·${reset} ${bold}${magenta}$(fmt_count "$git")${reset} ${label}git${reset}"
    [[ "$docs" -gt 0 ]] && seg+=" ${sep}·${reset} ${bold}${magenta}$(fmt_count "$docs")${reset} ${label}docs${reset}"
    [[ "$mem" -gt 0 ]] && seg+=" ${sep}·${reset} ${bold}${magenta}$(fmt_count "$mem")${reset} ${label}mem${reset}"
    [[ "$web" -gt 0 ]] && seg+=" ${sep}·${reset} ${bold}${magenta}$(fmt_count "$web")${reset} ${label}web${reset}"
    out+="$seg"
  fi

  out+="  ${sep}│${reset}  "
  out+="${bold}${magenta}$(fmt_count "$saved")${reset} ${label}saved${reset}"

  # Comms segment: unread (bright when >0, dim at zero) + identity in the full tier.
  if [[ -n "$comms_unread" ]]; then
    out+="  ${sep}│${reset}  "
    if [[ "$comms_unread" -gt 0 ]]; then
      out+="${bold}${green}✉ $(fmt_count "$comms_unread")${reset}"
    else
      out+="${muted}✉ 0${reset}"
    fi
    [[ "$tier" == "full" && -n "$comms_agent" ]] && out+=" ${muted}@${comms_agent:0:16}${reset}"
  fi
  printf '%s' "$out"
}

# ─── Emit ──────────────────────────────────────────────────────────────────────
ctx="$(build_context_line || true)"
bm="$(build_basemind_line)"
if [[ -n "$ctx" ]]; then
  printf '%s\n%s' "$ctx" "$bm"
else
  printf '%s' "$bm"
fi
