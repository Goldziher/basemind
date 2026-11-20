#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
STATUSLINE="$REPO_ROOT/.claude-plugin/statusline.sh"

# The checkout the binary is built from. Under poly's commit gate this script runs in a snapshot of
# the staged index, which holds no build output and whose files all carry the snapshot's creation
# time, so the sources and the binary are taken from the checkout the snapshot was made from. A
# linked worktree's git dir names its checkout in the `gitdir` file.
checkout_root() {
  local git_dir link
  git_dir="$(git -C "$REPO_ROOT" rev-parse --absolute-git-dir 2>/dev/null)" || git_dir=""
  if [[ -n "$git_dir" && -f "$git_dir/gitdir" ]]; then
    link="$(<"$git_dir/gitdir")"
    [[ "$link" == /* ]] || link="$git_dir/$link"
    git_dir="$link"
  fi
  if [[ -n "$git_dir" && -f "$(dirname "$git_dir")/Cargo.toml" ]]; then
    (cd "$(dirname "$git_dir")" && pwd)
  else
    printf '%s\n' "$REPO_ROOT"
  fi
}

CHECKOUT_ROOT="$(checkout_root)"

# poly's commit gate exports GIT_DIR and GIT_INDEX_FILE so a hook reaches the committing repository
# from the snapshot. Every repository below is a fixture: with those variables set, `git init`
# re-initialises the committing repository instead (marking it bare from a linked worktree), and
# basemind opens it in place of the fixture.
while IFS= read -r git_var; do
  unset "$git_var"
done < <(git rev-parse --local-env-vars)

[[ -x "$STATUSLINE" ]] || chmod +x "$STATUSLINE"

FIXTURE="$(mktemp -d)"
trap 'rm -rf "$FIXTURE"' EXIT

mkdir -p "$FIXTURE/.basemind/blobs"
mkdir -p "$FIXTURE/.basemind/views/working"
for i in 0 1 2 3 4 5 6; do
  : >"$FIXTURE/.basemind/blobs/${i}aaaaaaaa.fm.msgpack"
done
: >"$FIXTURE/.basemind/views/working/index.msgpack"

now_us="$(($(date +%s) * 1000000))"
# One row per bucket under the tool-name keys, plus rows an older binary wrote (`domain:mode` keys
# and a bare pre-consolidation name) so a telemetry log that spans an upgrade still buckets.
for tool in code_outline code_grep index_status git_log docs_search memory_search \
  code:outline git:recent memory:documents memory:get outline; do
  printf '{"ts_micros": %d, "tool": "%s", "est_tokens_saved": 500}\n' "$now_us" "$tool"
done >"$FIXTURE/.basemind/telemetry.jsonl"

payload="$(printf '{"workspace":{"current_dir":"%s"}}' "$FIXTURE")"
output="$(printf '%s' "$payload" | "$STATUSLINE")"
exit_code=$?

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

if command -v jq >/dev/null 2>&1; then
  # The per-bucket segment is rendered in the full tier only; pin it instead of relying on the width.
  plain="$(printf '%s' "$payload" | BASEMIND_STATUSLINE=full "$STATUSLINE" | sed $'s/\033\\[[0-9;]*m//g')"
  assert_plain_contains() {
    if [[ "$plain" == *"$1"* ]]; then
      printf '  ok  %s\n' "$2"
    else
      printf '  FAIL %s — expected %q in: %s\n' "$2" "$1" "$plain" >&2
      fail=1
    fi
  }
  assert_plain_contains '11 calls' 'every telemetry row counted'
  assert_plain_contains '5 srch' 'code_*, index_* and legacy code rows bucket as searches'
  assert_plain_contains '2 git' 'git_log and legacy git:recent bucket as git'
  assert_plain_contains '2 docs' 'docs_search and legacy memory:documents bucket as docs'
  assert_plain_contains '2 mem' 'memory_search and legacy memory:get bucket as mem'
else
  printf '  skip telemetry bucket assertions (jq unavailable)\n'
fi

plugin_version=""
if command -v jq >/dev/null 2>&1; then
  plugin_version="$(jq -r '.version // empty' "$REPO_ROOT/.claude-plugin/plugin.json" 2>/dev/null || true)"
fi
if [[ -n "$plugin_version" ]]; then
  assert_contains "v$plugin_version" "version v$plugin_version shown (full tier)"
  min_output="$(printf '%s' "$payload" | BASEMIND_STATUSLINE=minimal "$STATUSLINE")"
  if [[ "$min_output" != *"v$plugin_version"* ]]; then
    printf '  ok  version omitted in minimal tier\n'
  else
    printf '  FAIL minimal tier should omit version v%s; got: %q\n' "$plugin_version" "$min_output" >&2
    fail=1
  fi
  for word in 0 false No OFF; do
    nover_output="$(printf '%s' "$payload" | BASEMIND_STATUSLINE_VERSION="$word" "$STATUSLINE")"
    if [[ "$nover_output" != *"v$plugin_version"* ]]; then
      printf '  ok  BASEMIND_STATUSLINE_VERSION=%s hides version\n' "$word"
    else
      printf '  FAIL BASEMIND_STATUSLINE_VERSION=%s should hide version; got: %q\n' "$word" "$nover_output" >&2
      fail=1
    fi
  done
  for word in 1 TRUE yes on; do
    ver_output="$(printf '%s' "$payload" | BASEMIND_STATUSLINE_VERSION="$word" "$STATUSLINE")"
    if [[ "$ver_output" == *"v$plugin_version"* ]]; then
      printf '  ok  BASEMIND_STATUSLINE_VERSION=%s shows version\n' "$word"
    else
      printf '  FAIL BASEMIND_STATUSLINE_VERSION=%s should show version; got: %q\n' "$word" "$ver_output" >&2
      fail=1
    fi
  done
else
  printf '  skip version assertions (jq or plugin.json unavailable)\n'
fi

for name in BASEMIND_STATUSLINE_VERSION BASEMIND_STATUSLINE_CONTEXT; do
  bad_output="$(printf '%s' "$payload" | env "$name=maybe" "$STATUSLINE")"
  expected="$name=\"maybe\" is not a boolean: use 1, true, yes, on or 0, false, no, off"
  if [[ "$bad_output" == *"$expected"* ]]; then
    printf '  ok  %s=maybe is reported as an error naming it\n' "$name"
  else
    printf '  FAIL %s=maybe should be reported; got: %q\n' "$name" "$bad_output" >&2
    fail=1
  fi
done

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

empty_dir="$(mktemp -d)"
# Missing .basemind/ now delegates to `basemind statusline --root`. Shadow the binary with a stub that
# returns nothing, so this deterministically exercises the shell's own fallback (Part C: never blank)
# regardless of whatever basemind is installed on this machine.
stub_bin="$(mktemp -d)"
printf '#!/usr/bin/env bash\nexit 0\n' >"$stub_bin/basemind"
chmod +x "$stub_bin/basemind"
trap 'rm -rf "$FIXTURE" "$empty_dir" "$stub_bin"' EXIT
empty_payload="$(printf '{"workspace":{"current_dir":"%s"}}' "$empty_dir")"
empty_output="$(printf '%s' "$empty_payload" | PATH="$stub_bin:$PATH" "$STATUSLINE")"
if [[ "$empty_output" == *'no index'* ]] && [[ "$empty_output" == *'basemind scan'* ]]; then
  printf '  ok  missing .basemind/ + empty delegate output falls back to actionable hint\n'
else
  printf '  FAIL expected actionable hint, got: %q\n' "$empty_output" >&2
  fail=1
fi

resolver_dir="$(mktemp -d)"
mkdir -p \
  "$resolver_dir/0.9.0/.claude-plugin" \
  "$resolver_dir/0.14.0/.claude-plugin" \
  "$resolver_dir/0.19.0/.claude-plugin"
: >"$resolver_dir/0.19.0/.claude-plugin/statusline.sh"
: >"$resolver_dir/0.14.0/.claude-plugin/statusline.sh"
: >"$resolver_dir/0.9.0/.claude-plugin/statusline.sh"
touch "$resolver_dir/0.19.0/.claude-plugin/statusline.sh"
touch "$resolver_dir/0.14.0/.claude-plugin/statusline.sh"
touch "$resolver_dir/0.9.0/.claude-plugin/statusline.sh"
# shellcheck disable=SC2012  # must mirror the resolver, which uses `ls` verbatim
picked_v="$(ls -d "$resolver_dir"/*/.claude-plugin/statusline.sh 2>/dev/null | sort -V | tail -1)"
# shellcheck disable=SC2012  # mirrors the (buggy) mtime path we assert against
picked_mtime="$(ls -dt "$resolver_dir"/*/.claude-plugin/statusline.sh 2>/dev/null | head -1)"
rm -rf "$resolver_dir"
if [[ "$picked_v" == *"/0.19.0/"* ]]; then
  printf '  ok  resolver sort -V selects highest version (0.19.0)\n'
else
  printf '  FAIL resolver sort -V should pick 0.19.0; got: %q\n' "$picked_v" >&2
  fail=1
fi
if [[ "$picked_mtime" == *"/0.9.0/"* ]]; then
  printf '  ok  mtime ordering (ls -dt) demonstrably picks the WRONG version (0.9.0)\n'
else
  printf '  note mtime ordering picked %q (fixture timing); sort -V guard still holds\n' "$picked_mtime"
fi

# Global-cache mode: no in-repo .basemind/. The shell must delegate to `basemind statusline --root`,
# which reads the status.json sidecar the scanner writes into the machine-global cache. Needs the
# real binary (for the real workspace_key); skips cleanly if it can't be built.

# A binary is current when no build input of the checkout is newer than it.
binary_is_current() {
  local bin="$1" root="$2" input
  [[ -x "$bin" ]] || return 1
  for input in src build.rs Cargo.toml Cargo.lock; do
    [[ -e "$root/$input" ]] || continue
    [[ -z "$(find "$root/$input" -newer "$bin" -print -quit 2>/dev/null)" ]] || return 1
  done
}

gc_root="$CHECKOUT_ROOT"
gc_target="${CARGO_TARGET_DIR:-$gc_root/target}"
gc_bin=""
for candidate in "$gc_target/release/basemind" "$gc_target/debug/basemind"; do
  if binary_is_current "$candidate" "$gc_root"; then
    gc_bin="$candidate"
    printf '  ..  reusing %s (current with %s)\n' "$gc_bin" "$gc_root"
    break
  fi
done
if [[ -z "$gc_bin" ]] && command -v cargo >/dev/null 2>&1; then
  printf '  ..  building basemind into %s for the global-cache-mode check\n' "$gc_target"
  # Built in the checkout, where cargo's fingerprints hold, so only what changed is rebuilt.
  if (cd "$gc_root" && CARGO_TARGET_DIR="$gc_target" cargo build --quiet --features comms --bin basemind); then
    gc_bin="$gc_target/debug/basemind"
  fi
fi

if [[ -x "$gc_bin" ]]; then
  # The binary under test is a different build from the one serving this machine. Without its own
  # comms directory it takes over the live daemon's socket.
  BASEMIND_COMMS_DIR="$(mktemp -d)"
  export BASEMIND_COMMS_DIR
  gc_data="$(mktemp -d)"
  gc_repo="$(mktemp -d)"
  printf 'fn main() { println!("a"); }\n' >"$gc_repo/a.rs"
  printf 'pub fn b() -> i32 { 1 }\n' >"$gc_repo/b.rs"
  printf 'pub fn c() -> i32 { 2 }\n' >"$gc_repo/c.rs"
  git -C "$gc_repo" init -q >/dev/null 2>&1 || true
  BASEMIND_DATA_HOME="$gc_data" "$gc_bin" scan --root "$gc_repo" >/dev/null 2>&1 || true

  gc_keydir="$(find "$gc_data/cache/workspaces" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | head -1 || true)"
  if [[ -n "$gc_keydir" && -f "$gc_keydir/status.json" ]]; then
    printf '  ok  scan wrote status.json sidecar into the global-cache workspace dir\n'
  else
    printf '  FAIL scan did not write a status.json sidecar under %q\n' "$gc_data" >&2
    fail=1
  fi

  if [[ -n "$gc_keydir" ]]; then
    gc_now_us="$(($(date +%s) * 1000000))"
    printf '{"ts_micros":%d,"tool":"outline","resp_bytes":10,"est_tokens_saved":500,"saved_baseline":"read"}\n' \
      "$gc_now_us" >"$gc_keydir/telemetry.jsonl"
  fi

  # The shell resolves the repo root upward, so point current_dir at the scanned root itself. Put the
  # real binary first on PATH so the shell's `command -v basemind` finds it, and export the isolated
  # cache so `statusline --root` reads the fixture we just built.
  gc_payload="$(printf '{"workspace":{"current_dir":"%s"}}' "$gc_repo")"
  gc_output="$(printf '%s' "$gc_payload" |
    PATH="$(dirname "$gc_bin"):$PATH" BASEMIND_DATA_HOME="$gc_data" "$STATUSLINE")"
  gc_clean="$(printf '%s' "$gc_output" | sed -E $'s/\033\\[[0-9;:]*m//g')"
  rm -rf "$gc_data" "$gc_repo"

  if [[ "$gc_clean" == *'◆'* && "$gc_clean" == *'basemind'* ]]; then
    printf '  ok  global-cache mode renders the basemind brand line (delegated)\n'
  else
    printf '  FAIL global-cache mode should render the brand line; got: %q\n' "$gc_clean" >&2
    fail=1
  fi
  if [[ "$gc_clean" == *'3 files'* ]]; then
    printf '  ok  global-cache mode shows the sidecar file count (3 files)\n'
  else
    printf '  FAIL global-cache mode should show "3 files"; got: %q\n' "$gc_clean" >&2
    fail=1
  fi
  if [[ "$gc_clean" == *'no index'* ]]; then
    printf '  FAIL global-cache mode wrongly fell back to the no-index hint; got: %q\n' "$gc_clean" >&2
    fail=1
  else
    printf '  ok  global-cache mode does NOT show the stale "no index" hint (bug fixed)\n'
  fi

  # Real binary, isolated empty cache, unscanned repo → the delegated line is the actionable
  # "no index" hint (Part B's no-sidecar branch), proving `--root` never touches the daemon.
  gc_data2="$(mktemp -d)"
  gc_repo2="$(mktemp -d)"
  gc_payload2="$(printf '{"workspace":{"current_dir":"%s"}}' "$gc_repo2")"
  gc_output2="$(printf '%s' "$gc_payload2" |
    PATH="$(dirname "$gc_bin"):$PATH" BASEMIND_DATA_HOME="$gc_data2" "$STATUSLINE")"
  gc_clean2="$(printf '%s' "$gc_output2" | sed -E $'s/\033\\[[0-9;:]*m//g')"
  rm -rf "$gc_repo2"
  if [[ "$gc_clean2" == *'no index'* && "$gc_clean2" == *'basemind scan'* ]]; then
    printf '  ok  unscanned repo delegates to the actionable no-index hint (real binary)\n'
  else
    printf '  FAIL unscanned repo should delegate to no-index hint; got: %q\n' "$gc_clean2" >&2
    fail=1
  fi
  BASEMIND_DATA_HOME="$gc_data2" "$gc_bin" comms stop >/dev/null 2>&1 || true
  rm -rf "$gc_data2" "$BASEMIND_COMMS_DIR"
  unset BASEMIND_COMMS_DIR
else
  printf '  skip global-cache-mode check (no basemind binary and no cargo to build one)\n'
fi

if [[ $fail -eq 0 ]]; then
  printf 'statusline_smoke: all checks passed\n'
  exit 0
else
  printf 'statusline_smoke: FAILED\n' >&2
  printf '  rendered output: %q\n' "$output" >&2
  exit 1
fi
