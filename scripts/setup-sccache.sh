#!/usr/bin/env bash
# Route cargo's rustc through sccache so every worktree shares one compile cache. Idempotent: install
# sccache when missing, then add `rustc-wrapper` to the [build] table of cargo's user config unless
# that file already sets one.
set -euo pipefail

command -v sccache >/dev/null 2>&1 || mise install sccache
bin="$(command -v sccache || mise which sccache)"
home="${CARGO_HOME:-$HOME/.cargo}"
mkdir -p "$home"
# Cargo reads the extensionless file when both exist, so edit the one it reads.
cfg="$home/config.toml"
[ -f "$home/config" ] && cfg="$home/config"

# TOML allows a quoted key ("rustc-wrapper" =), a dotted key (build.rustc-wrapper =) and an indented
# table header ("  [build]").
quote="[\"']"
key="${quote}?rustc-wrapper${quote}?"
if grep -Eq "^[[:space:]]*(${quote}?build${quote}?[[:space:]]*\\.[[:space:]]*)?${key}[[:space:]]*=" "$cfg" 2>/dev/null; then
  echo "$cfg already sets a rustc-wrapper; left as it is"
else
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  next="$work/config.toml"
  src="$cfg"
  [ -f "$src" ] || src=/dev/null
  # sccache cannot cache incremental builds, so cargo's incremental mode is turned off with it.
  # An existing [build] table gets the keys (a second table would be invalid TOML); else one is added.
  build_header='^[[:space:]]*\[build\][[:space:]]*(#.*)?$'
  if grep -Eq "$build_header" "$src"; then
    BUILD_HEADER="$build_header" awk -v bin="$bin" '
      $0 ~ /^[[:space:]]*\[/ { in_build = 0 }
      in_build && /^[[:space:]]*incremental[[:space:]]*=/ { next }
      { print }
      $0 ~ ENVIRON["BUILD_HEADER"] { in_build = 1; printf "rustc-wrapper = \"%s\"\nincremental = false\n", bin }
    ' "$src" >"$next"
  else
    {
      cat "$src"
      printf '\n[build]\nrustc-wrapper = "%s"\nincremental = false\n' "$bin"
    } >"$next"
  fi
  # Cargo itself rejects a malformed file here, before it replaces the working one.
  cargo --config "$next" metadata --no-deps --format-version 1 >/dev/null
  cp "$next" "$cfg"
  echo "cargo now builds through $bin ($cfg)"
fi
"$bin" --start-server >/dev/null 2>&1 || true
"$bin" --show-stats | head -4
