#!/usr/bin/env bash

set -euo pipefail

input="$(cat 2>/dev/null || true)"
have_jq=0
command -v jq >/dev/null 2>&1 && have_jq=1

json() {
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

brand=$'\033[38;2;249;115;22m'
cyan=$'\033[38;5;51m'
green=$'\033[38;5;46m'
label=$'\033[38;5;255m'
sep=$'\033[38;5;240m'
muted=$'\033[38;5;244m'
bold=$'\033[1m'
reset=$'\033[0m'
glyph="◆"

fmt_count() {
  local n="$1"
  if [[ "$n" -lt 1000 ]]; then
    printf '%d' "$n"
  elif [[ "$n" -lt 10000 ]]; then
    printf '%d.%dk' "$((n / 1000))" "$(((n * 10 / 1000) % 10))"
  elif [[ "$n" -lt 1000000 ]]; then
    printf '%dk' "$((n / 1000))"
  else printf '%dM' "$((n / 1000000))"; fi
}
# A copy of the plugin hooks' run_bounded.
# Run "$@" for at most $1 seconds. Uses `timeout` where installed; macOS has none by default, so a
# watchdog stops the command there. The command runs in a process group of its own and the watchdog
# signals that group: a helper it left holding stdout goes too, while the daemon its CLI may have
# started, in a session of its own, is outside the group and keeps running. Standard input is passed
# through.
run_bounded() (
  secs="$1"
  shift
  if command -v timeout >/dev/null 2>&1; then
    exec timeout "$secs" "$@"
  fi
  # Job control gives a background job its own process group. The shell's job reports would go to
  # its stderr, so that is discarded and the command gets the caller's stderr from fd 3.
  exec 3>&2 2>/dev/null
  set -m
  "$@" <&0 2>&3 3>&- &
  pid=$!
  (
    nap=""
    trap '[ -z "$nap" ] || kill "$nap"; exit 0' TERM
    sleep "$secs" &
    nap=$!
    wait "$nap" || exit 0
    kill -TERM -- "-$pid"
  ) >/dev/null 3>&- &
  watchdog=$!
  wait "$pid"
  rc=$?
  kill "$watchdog"
  wait "$watchdog"
  exit "$rc"
)

# A directory only this user can write: $XDG_RUNTIME_DIR/basemind, else ${TMPDIR:-/tmp}/basemind-<uid>.
# Fails when the directory is a link, another user's, or open to others, so no state is written
# through a path another local user planted in a shared temporary directory.
private_state_dir() {
  local dir="${XDG_RUNTIME_DIR:-}"
  if [[ -n "$dir" && -d "$dir" && ! -L "$dir" && -O "$dir" ]]; then
    dir="$dir/basemind"
  else
    dir="${TMPDIR:-/tmp}"
    dir="${dir%/}/basemind-$(id -u)"
  fi
  mkdir -m 700 "$dir" 2>/dev/null || true
  [[ -d "$dir" && ! -L "$dir" && -O "$dir" ]] || return 1
  case "$(ls -ld "$dir" 2>/dev/null)" in
  drwx------*) printf '%s' "$dir" ;;
  *) return 1 ;;
  esac
}

comms_state() {
  command -v basemind >/dev/null 2>&1 || return 1
  command -v pgrep >/dev/null 2>&1 && pgrep -f "comms daemon" >/dev/null 2>&1 || return 1
  [[ $have_jq -eq 1 ]] || return 1

  local cache now cts cu ca json unread agent name state_dir tmp
  # The actor this session's agent tools act as: the plugin's agent-as hook names the main session
  # from its session id, under the principal `serve` derives with BASEMIND_HOST=claude.
  name="$(printf '%s' "$input" | run_bounded 2 env BASEMIND_HOST=claude basemind agent hook name 2>/dev/null | tr -cd 'a-z0-9_-' || true)"
  cache=""
  if state_dir="$(private_state_dir)"; then
    cache="${state_dir}/sl-comms-$(printf '%s %s' "$cwd" "$name" | cksum | cut -d' ' -f1)"
  fi
  now="$(date +%s)"
  if [[ -n "$cache" && -f "$cache" && ! -L "$cache" ]]; then
    read -r cts cu ca <"$cache" 2>/dev/null || true
    if [[ -n "${cts:-}" && "$cts" =~ ^[0-9]+$ ]] && ((now - cts < 8)); then
      printf '%s %s' "${cu:-0}" "${ca:-}"
      return 0
    fi
  fi

  local as=()
  [[ -n "$name" ]] && as=(--as "$name")
  json="$(run_bounded 2 env BASEMIND_HOST=claude basemind agent inbox --peek ${as[@]+"${as[@]}"} --root "$cwd" --json --limit 1 </dev/null 2>/dev/null || true)"
  [[ -n "$json" ]] || return 1
  unread="$(printf '%s' "$json" | jq -r '(.messages | length)' 2>/dev/null | tr -cd '0-9' || true)"
  [[ -n "$unread" ]] || unread=0
  agent="${BASEMIND_AGENT_ID:-${name}}"
  if [[ -n "$cache" ]] && tmp="$(mktemp "${cache}.XXXXXX" 2>/dev/null)"; then
    printf '%s %s %s\n' "$now" "$unread" "${agent:-}" >"$tmp" 2>/dev/null && mv -f "$tmp" "$cache" 2>/dev/null ||
      rm -f "$tmp" 2>/dev/null || true
  fi
  printf '%s %s' "$unread" "${agent:-}"
}

build_context_line() {
  [[ "${BASEMIND_STATUSLINE_CONTEXT:-1}" == "0" ]] && return 1
  [[ -z "$model" && -z "$out_style" ]] && return 1

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

mark() { printf '%s%s%s %s%sbasemind%s' "$brand" "$glyph" "$reset" "$bold" "$brand" "$reset"; }

bm_version() {
  [[ "${BASEMIND_STATUSLINE_VERSION:-1}" == "0" ]] && return 0
  local dir ver="" parent manifest
  dir="$(cd "$(dirname "$0")" 2>/dev/null && pwd)" || return 0
  # Beside the manifest (`.claude-plugin/statusline.sh`) or in the plugin's `scripts/`.
  for manifest in "$dir/plugin.json" "$dir/../.claude-plugin/plugin.json"; do
    if [[ -z "$ver" && $have_jq -eq 1 && -f "$manifest" ]]; then
      ver="$(jq -r '.version // empty' "$manifest" 2>/dev/null || true)"
    fi
  done
  if [[ -z "$ver" ]]; then
    parent="$(basename "$(dirname "$dir")")"
    [[ "$parent" =~ ^[0-9]+\.[0-9]+ ]] && ver="$parent"
  fi
  [[ -n "$ver" ]] && printf 'v%s' "$ver"
}

# The unread-message segment, empty when the comms daemon is not running or cannot be asked.
comms_segment() {
  local raw unread agent
  raw="$(comms_state 2>/dev/null)" || return 0
  unread="${raw%% *}"
  agent="${raw#* }"
  [[ "$agent" == "$raw" ]] && agent=""
  [[ "$unread" =~ ^[0-9]+$ ]] || return 0
  if [[ "$tier" == "minimal" ]]; then
    if [[ "$unread" -gt 0 ]]; then
      printf ' %s·%s %s%s✉%s%s' "$sep" "$reset" "$bold" "$green" "$unread" "$reset"
    fi
    return 0
  fi
  printf '  %s│%s  ' "$sep" "$reset"
  if [[ "$unread" -gt 0 ]]; then
    printf '%s%s✉ %s%s' "$bold" "$green" "$(fmt_count "$unread")" "$reset"
  else
    printf '%s✉ 0%s' "$muted" "$reset"
  fi
  if [[ "$tier" == "full" && -n "$agent" ]]; then
    printf ' %s@%s%s' "$muted" "${agent:0:16}" "$reset"
  fi
  return 0
}

# The index lives in the machine-global cache under a hash of the canonical repository path the
# shell cannot reproduce, so the binary renders the line from the cheap status.json sidecar without
# opening the index; the plugin version is spliced in after its brand mark. An in-repo `.basemind/`
# holds at most a fallback config and is not read here.
build_basemind_line() {
  local out="" brand_mark ver
  if command -v basemind >/dev/null 2>&1; then
    out="$(run_bounded 2 basemind statusline --root "$cwd" </dev/null 2>/dev/null || true)"
  fi
  brand_mark="$(mark)"
  if [[ -z "$out" ]]; then
    out="$(printf '%s %s│%s %sno index — run:%s %s%sbasemind scan%s' \
      "$brand_mark" "$sep" "$reset" "$label" "$reset" "$bold" "$cyan" "$reset")"
  elif [[ "$tier" != "minimal" && "$out" == "$brand_mark"* ]]; then
    ver="$(bm_version || true)"
    if [[ -n "$ver" ]]; then
      out="${brand_mark}  ${bold}${muted}${ver}${reset}  ${sep}│${reset}${out#"$brand_mark"}"
    fi
  fi
  printf '%s%s' "$out" "$(comms_segment)"
}

ctx="$(build_context_line || true)"
bm="$(build_basemind_line)"
if [[ -n "$ctx" ]]; then
  printf '%s\n%s' "$ctx" "$bm"
else
  printf '%s' "$bm"
fi
