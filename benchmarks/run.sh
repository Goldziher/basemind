#!/usr/bin/env bash
# benchmarks/run.sh <task-file.yaml>
#
# Runs each task's `basemind` command and `baseline` command, counts the real
# token cost of each side's stdout with `basemind admin tokens --stdin` (the
# actual o200k tokenizer, not the `bytes/4` heuristic in src/mcp/savings.rs),
# times both, and prints a report table. See benchmarks/README.md for the task
# file format and prerequisites.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BASEMIND_BIN="${BASEMIND_BIN:-$REPO_ROOT/target/debug/basemind}"

die() {
  echo "error: $*" >&2
  exit 1
}

[[ $# -eq 1 ]] || die "usage: $0 <task-file.yaml>"
TASK_FILE="$1"
[[ -f "$TASK_FILE" ]] || die "task file not found: $TASK_FILE"
[[ -x "$BASEMIND_BIN" ]] || die "basemind binary not found or not executable: $BASEMIND_BIN
  Build it first: cargo build --bin basemind --features tokenizer (from $REPO_ROOT)"

command -v python3 >/dev/null 2>&1 || die "python3 is required to parse the YAML task file"
command -v jq >/dev/null 2>&1 || die "jq is required to iterate the parsed task list"
python3 -c 'import yaml' >/dev/null 2>&1 || die "python3's pyyaml module is required (pip install pyyaml)"

# --- Sanity check #1: the tokenizer primitive both sides of every row depend on. ---
TOKENS_PROBE="$(printf 'hello world' | "$BASEMIND_BIN" admin tokens --stdin 2>/tmp/basemind_bench_tokens_err.$$)"
TOKENS_STATUS=$?
if [[ $TOKENS_STATUS -ne 0 || -z "$TOKENS_PROBE" ]]; then
  echo "error: '$BASEMIND_BIN admin tokens --stdin' failed (exit $TOKENS_STATUS)." >&2
  echo "This benchmark scores both sides with the real o200k tokenizer, so it refuses to" >&2
  echo "fall back to a byte-count heuristic. Rebuild with the tokenizer feature:" >&2
  echo "  cargo build --bin basemind --features tokenizer   # or --features documents" >&2
  echo "--- stderr from the probe call ---" >&2
  cat /tmp/basemind_bench_tokens_err.$$ >&2
  rm -f /tmp/basemind_bench_tokens_err.$$
  exit 1
fi
rm -f /tmp/basemind_bench_tokens_err.$$

# --- Sanity check #2: the index must actually have files in it. ---
STATUS_JSON="$("$BASEMIND_BIN" status --json 2>/dev/null)" || die \
  "'$BASEMIND_BIN status --json' failed; is $REPO_ROOT a basemind-indexed repo?"
FILE_COUNT="$(echo "$STATUS_JSON" | jq -r '.file_count // 0')"
if [[ "$FILE_COUNT" -eq 0 ]]; then
  die "index has 0 files. Run '$BASEMIND_BIN admin scan' first, then re-run this benchmark."
fi

count_tokens() {
  "$BASEMIND_BIN" admin tokens --stdin 2>/dev/null <<<"$1"
}

now_ns() { date +%s%N; }

# Runs "$1" through bash -c, capturing stdout only (stderr discarded so log
# noise never gets tokenized alongside real output). Prints "<ms>\t<stdout>".
run_timed() {
  local cmd="$1" start end out
  start="$(now_ns)"
  out="$(bash -c "$cmd" 2>/dev/null)"
  end="$(now_ns)"
  printf '%d\t%s' "$(((end - start) / 1000000))" "$out"
}

TASKS_JSON="$(python3 -c '
import json, sys, yaml
with open(sys.argv[1]) as f:
    data = yaml.safe_load(f)
print(json.dumps(data.get("tasks", [])))
' "$TASK_FILE")" || die "failed to parse $TASK_FILE as YAML"

TASK_COUNT="$(echo "$TASKS_JSON" | jq 'length')"
[[ "$TASK_COUNT" -gt 0 ]] || die "no tasks found in $TASK_FILE"

printf '%-32s %-12s %10s %10s %8s %10s %10s\n' \
  "task" "tier" "bm_tok" "base_tok" "delta%" "bm_ms" "base_ms"
printf '%s\n' "--------------------------------------------------------------------------------------------------"

TOTAL_BM_TOK=0
TOTAL_BASE_TOK=0
TOTAL_BM_MS=0
TOTAL_BASE_MS=0

for i in $(seq 0 $((TASK_COUNT - 1))); do
  TASK="$(echo "$TASKS_JSON" | jq -c ".[$i]")"
  NAME="$(echo "$TASK" | jq -r '.name')"
  TIER="$(echo "$TASK" | jq -r '.tier')"
  BM_CMD="$(echo "$TASK" | jq -r '.basemind')"
  BASE_CMD="$(echo "$TASK" | jq -r '.baseline')"

  BM_RESULT="$(run_timed "\"$BASEMIND_BIN\" $BM_CMD")"
  BM_MS="${BM_RESULT%%$'\t'*}"
  BM_OUT="${BM_RESULT#*$'\t'}"

  BASE_RESULT="$(run_timed "$BASE_CMD")"
  BASE_MS="${BASE_RESULT%%$'\t'*}"
  BASE_OUT="${BASE_RESULT#*$'\t'}"

  BM_TOK="$(count_tokens "$BM_OUT")"
  if [[ -z "$BM_TOK" ]]; then
    die "'$BASEMIND_BIN admin tokens --stdin' failed while scoring task '$NAME'. \
See the sanity check above — this should not happen once the probe passes."
  fi
  BASE_TOK="$(count_tokens "$BASE_OUT")"
  if [[ -z "$BASE_TOK" ]]; then
    die "'$BASEMIND_BIN admin tokens --stdin' failed while scoring the baseline side of task '$NAME'."
  fi

  if [[ "$BASE_TOK" -gt 0 ]]; then
    DELTA="$(awk -v b="$BASE_TOK" -v m="$BM_TOK" 'BEGIN { printf "%.1f", (b - m) / b * 100 }')"
  else
    DELTA="n/a"
  fi

  printf '%-32s %-12s %10s %10s %7s%% %9sms %9sms\n' \
    "$NAME" "$TIER" "$BM_TOK" "$BASE_TOK" "$DELTA" "$BM_MS" "$BASE_MS"

  TOTAL_BM_TOK=$((TOTAL_BM_TOK + BM_TOK))
  TOTAL_BASE_TOK=$((TOTAL_BASE_TOK + BASE_TOK))
  TOTAL_BM_MS=$((TOTAL_BM_MS + BM_MS))
  TOTAL_BASE_MS=$((TOTAL_BASE_MS + BASE_MS))
done

printf '%s\n' "--------------------------------------------------------------------------------------------------"
if [[ "$TOTAL_BASE_TOK" -gt 0 ]]; then
  TOTAL_DELTA="$(awk -v b="$TOTAL_BASE_TOK" -v m="$TOTAL_BM_TOK" 'BEGIN { printf "%.1f", (b - m) / b * 100 }')"
else
  TOTAL_DELTA="n/a"
fi
printf '%-32s %-12s %10s %10s %7s%% %9sms %9sms\n' \
  "TOTAL" "($TASK_COUNT tasks)" "$TOTAL_BM_TOK" "$TOTAL_BASE_TOK" "$TOTAL_DELTA" "$TOTAL_BM_MS" "$TOTAL_BASE_MS"
