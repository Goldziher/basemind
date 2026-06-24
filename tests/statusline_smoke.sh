#!/usr/bin/env bash
# Smoke test for .claude-plugin/statusline.sh — runs the script against a
# synthetic fixture and asserts the rendered output has colors, the brand mark,
# a file count derived from the blob store, and the freshness dot.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
STATUSLINE="$REPO_ROOT/.claude-plugin/statusline.sh"

[[ -x "$STATUSLINE" ]] || chmod +x "$STATUSLINE"

# ─── Fixture ───────────────────────────────────────────────────────────────────
FIXTURE="$(mktemp -d)"
trap 'rm -rf "$FIXTURE"' EXIT

mkdir -p "$FIXTURE/.basemind/blobs"
mkdir -p "$FIXTURE/.basemind/views/working"
# Synthesize 7 fake filemap blobs → file_count == 7.
for i in 0 1 2 3 4 5 6; do
  : >"$FIXTURE/.basemind/blobs/${i}aaaaaaaa.fm.msgpack"
done
# A views index file so scan-age stamps as "Xs ago".
: >"$FIXTURE/.basemind/views/working/index.msgpack"

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
assert_contains $'\033[38;2;249;115;22m' 'true-color brand orange #F97316 present'
assert_contains '◆' 'brand glyph ◆ present'
assert_contains 'basemind' 'name present'
assert_contains '●' 'liveness dot present'
assert_contains '7' 'file count 7 from blob fixture'

# Legacy split-layout index (pre-0.9 `.l1`/`.l2` blobs, still written by an older serve)
# must read as ready — NOT stuck on "scanning…" — and count one per source file (the
# `.l2` secondary layer is never counted). Regression guard for the `.fm`-only check that
# left every older index showing "scanning…" forever.
legacy_dir="$(mktemp -d)"
mkdir -p "$legacy_dir/.basemind/blobs" "$legacy_dir/.basemind/views/working"
for i in 0 1 2 3; do
  : >"$legacy_dir/.basemind/blobs/${i}bbbbbbbb.l1.msgpack"
  : >"$legacy_dir/.basemind/blobs/${i}bbbbbbbb.l2.msgpack"
done
: >"$legacy_dir/.basemind/views/working/index.msgpack"
legacy_payload="$(printf '{"workspace":{"current_dir":"%s"}}' "$legacy_dir")"
legacy_output="$(printf '%s' "$legacy_payload" | "$STATUSLINE")"
legacy_clean="$(printf '%s' "$legacy_output" | sed -E $'s/\033\\[[0-9;:]*m//g')"
rm -rf "$legacy_dir"
if [[ "$legacy_clean" != *'scanning'* ]] && [[ "$legacy_clean" == *'4 files'* ]]; then
  printf '  ok  legacy .l1/.l2 index renders ready with 4 files (no double-count)\n'
else
  printf '  FAIL legacy index should render "4 files" and not scanning; got: %q\n' "$legacy_clean" >&2
  fail=1
fi

# Empty-index (`.basemind/` exists but no blobs/) → "scanning…" hint.
unscanned_dir="$(mktemp -d)"
mkdir -p "$unscanned_dir/.basemind"
trap 'rm -rf "$FIXTURE" "$empty_dir" "$unscanned_dir"' EXIT
unscanned_payload="$(printf '{"workspace":{"current_dir":"%s"}}' "$unscanned_dir")"
unscanned_output="$(printf '%s' "$unscanned_payload" | "$STATUSLINE")"
if [[ "$unscanned_output" == *'scanning'* ]]; then
  printf '  ok  unscanned (no blobs) renders scanning hint\n'
else
  printf '  FAIL unscanned output should say scanning; got: %q\n' "$unscanned_output" >&2
  fail=1
fi

# Missing `.basemind/` → actionable "no index — run: basemind scan" hint.
empty_dir="$(mktemp -d)"
trap 'rm -rf "$FIXTURE" "$empty_dir"' EXIT
empty_payload="$(printf '{"workspace":{"current_dir":"%s"}}' "$empty_dir")"
empty_output="$(printf '%s' "$empty_payload" | "$STATUSLINE")"
if [[ "$empty_output" == *'no index'* ]] && [[ "$empty_output" == *'basemind scan'* ]]; then
  printf '  ok  missing .basemind/ shows actionable hint\n'
else
  printf '  FAIL expected actionable hint, got: %q\n' "$empty_output" >&2
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
