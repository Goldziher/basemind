#!/usr/bin/env bash
# Exercises scripts/setup-sccache.sh against throwaway cargo homes with stub `sccache` and `cargo`.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SETUP="$SCRIPT_DIR/../scripts/setup-sccache.sh"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/bin"
printf '#!/bin/sh\nexit 0\n' >"$TMP/bin/sccache"
printf '#!/bin/sh\nexit 0\n' >"$TMP/bin/cargo"
chmod +x "$TMP/bin/sccache" "$TMP/bin/cargo"

failures=0
fail() {
  echo "FAIL: $1" >&2
  failures=$((failures + 1))
}

# run_case <name> <initial config text, or "-" for none>; the result is $TMP/<name>/config.toml
run_case() {
  local name="$1" initial="$2" home="$TMP/$1"
  mkdir -p "$home"
  [[ "$initial" == "-" ]] || printf '%s\n' "$initial" >"$home/config.toml"
  PATH="$TMP/bin:$PATH" CARGO_HOME="$home" bash "$SETUP" >"$home/out" 2>&1 || fail "$name: script exited non-zero"
}

wrapper_count() { grep -Ec '^[[:space:]]*"?rustc-wrapper"?[[:space:]]*=' "$TMP/$1/config.toml" || true; }

# An existing wrapper in any TOML spelling is left untouched.
keep_names=(quoted single-quoted dotted quoted-dotted indented)
keep_bodies=(
  $'[build]\n"rustc-wrapper" = "/x/other"'
  $'[build]\n\'rustc-wrapper\' = "/x/other"'
  'build.rustc-wrapper = "/x/other"'
  '"build"."rustc-wrapper" = "/x/other"'
  $'[build]\n    rustc-wrapper = "/x/other"'
)
for i in "${!keep_names[@]}"; do
  name="keep-${keep_names[$i]}"
  run_case "$name" "${keep_bodies[$i]}"
  [[ "$(<"$TMP/$name/config.toml")" == "${keep_bodies[$i]}" ]] || fail "$name: config was rewritten"
done

# An indented header is a table boundary and an indented [build] is the [build] table.
run_case indented-build $'  [build]\nincremental = true\n  [alias]\nincremental = "x"'
cfg="$TMP/indented-build/config.toml"
[[ "$(wrapper_count indented-build)" == 1 ]] || fail "indented-build: expected exactly one rustc-wrapper"
[[ "$(grep -Ec '^[[:space:]]*\[build\]' "$cfg")" == 1 ]] || fail "indented-build: [build] table duplicated"
grep -q '^incremental = "x"$' "$cfg" || fail "indented-build: a key under the next indented header was deleted"
grep -q 'incremental = true' "$cfg" && fail "indented-build: old incremental key kept in [build]"

# No config at all, and a config without [build], get a fresh [build] table.
run_case fresh -
[[ "$(wrapper_count fresh)" == 1 ]] || fail "fresh: expected one rustc-wrapper"
run_case no-build $'[alias]\nb = "build"'
[[ "$(wrapper_count no-build)" == 1 ]] || fail "no-build: expected one rustc-wrapper"
grep -q '^b = "build"$' "$TMP/no-build/config.toml" || fail "no-build: existing table lost"

# A second run changes nothing.
before="$(<"$TMP/fresh/config.toml")"
PATH="$TMP/bin:$PATH" CARGO_HOME="$TMP/fresh" bash "$SETUP" >/dev/null 2>&1
[[ "$(<"$TMP/fresh/config.toml")" == "$before" ]] || fail "fresh: second run rewrote the config"

if ((failures > 0)); then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "setup_sccache_smoke: ok"
