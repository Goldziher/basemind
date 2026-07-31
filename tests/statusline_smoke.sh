#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
STATUSLINE="$REPO_ROOT/.claude-plugin/statusline.sh"

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
printf '{"ts_micros": %d, "tool": "outline", "est_tokens_saved": 500}\n' "$now_us" \
	>"$FIXTURE/.basemind/telemetry.jsonl"

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
	nover_output="$(printf '%s' "$payload" | BASEMIND_STATUSLINE_VERSION=0 "$STATUSLINE")"
	if [[ "$nover_output" != *"v$plugin_version"* ]]; then
		printf '  ok  BASEMIND_STATUSLINE_VERSION=0 hides version\n'
	else
		printf '  FAIL BASEMIND_STATUSLINE_VERSION=0 should hide version; got: %q\n' "$nover_output" >&2
		fail=1
	fi
else
	printf '  skip version assertions (jq or plugin.json unavailable)\n'
fi

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
gc_bin=""
for candidate in "$REPO_ROOT/target/release/basemind" "$REPO_ROOT/target/debug/basemind"; do
	[[ -x "$candidate" ]] && gc_bin="$candidate" && break
done
if [[ -z "$gc_bin" ]] && command -v cargo >/dev/null 2>&1; then
	printf '  .. building basemind for global-cache-mode check\n'
	(cd "$REPO_ROOT" && cargo build --quiet --features comms --bin basemind) && gc_bin="$REPO_ROOT/target/debug/basemind"
fi

if [[ -x "$gc_bin" ]]; then
	gc_data="$(mktemp -d)"
	gc_repo="$(mktemp -d)"
	printf 'fn main() { println!("a"); }\n' >"$gc_repo/a.rs"
	printf 'pub fn b() -> i32 { 1 }\n' >"$gc_repo/b.rs"
	printf 'pub fn c() -> i32 { 2 }\n' >"$gc_repo/c.rs"
	git -C "$gc_repo" init -q >/dev/null 2>&1 || true
	BASEMIND_DATA_HOME="$gc_data" "$gc_bin" scan --root "$gc_repo" >/dev/null 2>&1 || true

	gc_keydir="$(find "$gc_data/cache/workspaces" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | head -1)"
	if [[ -n "$gc_keydir" && -f "$gc_keydir/status.json" ]]; then
		printf '  ok  scan wrote status.json sidecar into the global-cache workspace dir\n'
	else
		printf '  FAIL scan did not write a status.json sidecar under %q\n' "$gc_data" >&2
		fail=1
	fi

	gc_now_us="$(($(date +%s) * 1000000))"
	printf '{"ts_micros":%d,"tool":"outline","resp_bytes":10,"est_tokens_saved":500,"saved_baseline":"read"}\n' \
		"$gc_now_us" >"$gc_keydir/telemetry.jsonl"

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
	rm -rf "$gc_data2" "$gc_repo2"
	if [[ "$gc_clean2" == *'no index'* && "$gc_clean2" == *'basemind scan'* ]]; then
		printf '  ok  unscanned repo delegates to the actionable no-index hint (real binary)\n'
	else
		printf '  FAIL unscanned repo should delegate to no-index hint; got: %q\n' "$gc_clean2" >&2
		fail=1
	fi
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
