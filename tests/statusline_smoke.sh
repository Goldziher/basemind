#!/usr/bin/env bash
# Smoke test for .claude-plugin/statusline.sh — runs the script against a
# synthetic fixture and asserts the rendered output has colors, the brand mark,
# a file count, and the freshness dot.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
STATUSLINE="$REPO_ROOT/.claude-plugin/statusline.sh"

[[ -x "$STATUSLINE" ]] || chmod +x "$STATUSLINE"

# ─── Fixture ───────────────────────────────────────────────────────────────────
FIXTURE="$(mktemp -d)"
trap 'rm -rf "$FIXTURE"' EXIT

mkdir -p "$FIXTURE/.basemind/views/working"
# 13 kB → ~100 files at 130 bytes/entry
dd if=/dev/zero of="$FIXTURE/.basemind/views/working/index.msgpack" bs=1024 count=13 status=none

# Synthesize one telemetry record from today (microseconds since epoch).
now_us="$(($(date +%s) * 1000000))"
printf '{"ts_micros": %d, "tool": "outline", "est_tokens_saved": 500}\n' "$now_us" \
  >"$FIXTURE/.basemind/telemetry.jsonl"

# ─── Run ───────────────────────────────────────────────────────────────────────
payload="$(printf '{"workspace":{"current_dir":"%s"}}' "$FIXTURE")"
output="$(printf '%s' "$payload" | "$STATUSLINE")"
exit_code=$?

# ─── Assertions ────────────────────────────────────────────────────────────────
fail=0
assert_contains() {
  local needle="$1"
  local label="$2"
  if [[ "$output" == *"$needle"* ]]; then
    printf '  ok  %s\n' "$label"
  else
    printf '  FAIL %s — expected to contain %q\n' "$label" "$needle" >&2
    fail=1
  fi
}

if [[ $exit_code -eq 0 ]]; then
  printf '  ok  exit 0\n'
else
  printf '  FAIL non-zero exit: %d\n' "$exit_code" >&2
  fail=1
fi

assert_contains $'\033[' 'ANSI escape present'
assert_contains '▲' 'brand mark ▲ present'
assert_contains 'basemind' 'name present'
assert_contains 'files · scanned' 'layout glue present'
assert_contains '●' 'freshness dot present'
assert_contains 'calls' 'calls segment present'
assert_contains 'tok saved' 'tokens segment present'
assert_contains '1 calls' '1 call recorded (from fixture telemetry)'
assert_contains '500 tok saved' '500 tok recorded (from fixture telemetry)'

# Silent-exit when .basemind/ is missing.
empty_dir="$(mktemp -d)"
trap 'rm -rf "$FIXTURE" "$empty_dir"' EXIT
empty_payload="$(printf '{"workspace":{"current_dir":"%s"}}' "$empty_dir")"
empty_output="$(printf '%s' "$empty_payload" | "$STATUSLINE")"
if [[ -z "$empty_output" ]]; then
  printf '  ok  silent-exit when .basemind/ missing\n'
else
  printf '  FAIL expected empty output, got: %s\n' "$empty_output" >&2
  fail=1
fi

if [[ $fail -eq 0 ]]; then
  printf 'statusline_smoke: all checks passed\n'
  exit 0
else
  printf 'statusline_smoke: FAILED\n' >&2
  printf '  rendered output: %q\n' "$output" >&2
  exit 1
fi
