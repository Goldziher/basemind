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

epoch_mtime() {
	stat -c %Y "$1" 2>/dev/null || stat -f %m "$1" 2>/dev/null || echo 0
}

cwd="$(json '.workspace.current_dir // .cwd' "${PWD}")"
model="$(json '.model.display_name' '')"
out_style="$(json '.output_style.name' '')"
vim_mode="$(json '.vim.mode' '')"
ctx_pct="$(json '.context_window.used_percentage' '')"
bm_dir="${cwd}/.basemind"

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
magenta=$'\033[38;5;201m'
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
fmt_thousands() {
	local n="$1"
	LC_ALL=en_US.UTF-8 printf "%'d" "$n" 2>/dev/null || printf '%d' "$n"
}
fmt_kb() {
	local kb="$1"
	[[ -z "$kb" || "$kb" -le 0 ]] 2>/dev/null && {
		printf '0'
		return
	}
	if [[ "$kb" -lt 1024 ]]; then
		printf '%dK' "$kb"
	elif [[ "$kb" -lt 1048576 ]]; then
		printf '%dM' "$((kb / 1024))"
	else printf '%d.%dG' "$((kb / 1048576))" "$(((kb * 10 / 1048576) % 10))"; fi
}

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
	local dir ver="" parent
	dir="$(cd "$(dirname "$0")" 2>/dev/null && pwd)" || return 0
	if [[ $have_jq -eq 1 && -f "$dir/plugin.json" ]]; then
		ver="$(jq -r '.version // empty' "$dir/plugin.json" 2>/dev/null || true)"
	fi
	if [[ -z "$ver" ]]; then
		parent="$(basename "$(dirname "$dir")")"
		[[ "$parent" =~ ^[0-9]+\.[0-9]+ ]] && ver="$parent"
	fi
	[[ -n "$ver" ]] && printf 'v%s' "$ver"
}

build_basemind_line() {
	if [[ ! -d "$bm_dir" ]]; then
		printf '%s %s│%s %sno index — run:%s %s%sbasemind scan%s' \
			"$(mark)" "$sep" "$reset" "$label" "$reset" "$bold" "$cyan" "$reset"
		return
	fi
	local blobs_dir="${bm_dir}/blobs"
	file_blobs() { find "$blobs_dir" -maxdepth 1 -type f \( -name '*.fm.msgpack' -o -name '*.l1.msgpack' \) "$@" 2>/dev/null; }
	if [[ ! -d "$blobs_dir" ]] || [[ -z "$(file_blobs -print -quit)" ]]; then
		printf '%s %s│%s %sscanning…%s' "$(mark)" "$sep" "$reset" "$label" "$reset"
		return
	fi

	local file_count
	file_count="$(file_blobs | wc -l || echo 0)"
	file_count="${file_count##*[[:space:]]}"
	[[ -z "$file_count" ]] && file_count=0

	local scan_age="never" scan_delta=999999999 scan_mtime=0 m now
	for candidate in "${bm_dir}/views/working/index.msgpack" "${bm_dir}/views/working/index.fjall"; do
		if [[ -e "$candidate" ]]; then
			m="$(epoch_mtime "$candidate")"
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

	local calls=0 saved=0 code=0 git=0 docs=0 mem=0 web=0 tel_mtime=0
	local tel_file="${bm_dir}/telemetry.jsonl"
	if [[ -f "$tel_file" ]]; then
		tel_mtime="$(epoch_mtime "$tel_file")"
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

	local comms_unread="" comms_agent="" comms_raw
	comms_raw="$(comms_state 2>/dev/null || true)"
	if [[ -n "$comms_raw" ]]; then
		comms_unread="${comms_raw%% *}"
		comms_agent="${comms_raw#* }"
		[[ "$comms_agent" == "$comms_raw" ]] && comms_agent=""
	fi

	local searches=$((code))
	local out ver
	out="$(mark)  ${dot}  "
	if [[ "$tier" != "minimal" ]]; then
		ver="$(bm_version)"
		[[ -n "$ver" ]] && out+="${bold}${muted}${ver}${reset}  ${sep}│${reset}  "
	fi
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

	if [[ "$tier" == "full" ]]; then
		local disk_kb="" rss_kb="" res_seg=""
		res_add() {
			[[ -n "$res_seg" ]] && res_seg+=" ${sep}·${reset} "
			res_seg+="$1"
		}
		disk_kb="$(du -sk "$bm_dir" 2>/dev/null | awk '{print $1}')"
		[[ -n "$disk_kb" ]] && res_add "${bold}${cyan}$(fmt_kb "$disk_kb")${reset} ${label}disk${reset}"
		if [[ $serve_running -eq 1 ]] && command -v ps >/dev/null 2>&1; then
			local serve_pid
			serve_pid="$(pgrep -f "basemind serve" 2>/dev/null | head -1)"
			[[ -n "$serve_pid" ]] && rss_kb="$(ps -o rss= -p "$serve_pid" 2>/dev/null | tr -d ' ')"
			[[ -n "$rss_kb" && "$rss_kb" -gt 0 ]] 2>/dev/null &&
				res_add "${bold}${cyan}$(fmt_kb "$rss_kb")${reset} ${label}rss${reset}"
		fi
		[[ -n "$res_seg" ]] && out+="  ${sep}│${reset}  $res_seg"
	fi

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

ctx="$(build_context_line || true)"
bm="$(build_basemind_line)"
if [[ -n "$ctx" ]]; then
	printf '%s\n%s' "$ctx" "$bm"
else
	printf '%s' "$bm"
fi
