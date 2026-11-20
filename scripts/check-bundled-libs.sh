#!/usr/bin/env bash
# Fails when a release archive's staging directory bundles a native library that is not allowlisted.
#
# Usage: check-bundled-libs.sh <linux|macos|windows> <staging-dir>
#
# Linux and macOS bundle libraries under <staging-dir>/lib/; Windows puts DLLs next to the binary.
set -euo pipefail

# One basename per line. Every entry needs a licence check and its licence text shipped with the
# archive (#232) before it is added. Basenames are exact, so a soname bump fails the build and gets
# the same review. Libraries the loader takes from the host (glibc, libgcc_s, /usr/lib, /System) are
# never bundled and so never listed.
ALLOWED_LINUX='
libstdc++.so.6
'
ALLOWED_MACOS=''
ALLOWED_WINDOWS=''

if [ $# -ne 2 ]; then
  echo "Usage: $0 <linux|macos|windows> <staging-dir>" >&2
  exit 2
fi

system="$1"
staging="$2"

case "$system" in
linux)
  allowed="$ALLOWED_LINUX"
  bundled=("$staging"/lib/*)
  ;;
macos)
  allowed="$ALLOWED_MACOS"
  bundled=("$staging"/lib/*)
  ;;
windows)
  allowed="$ALLOWED_WINDOWS"
  bundled=("$staging"/*.dll)
  ;;
*)
  echo "Unknown system: $system (expected linux, macos or windows)" >&2
  exit 2
  ;;
esac

unlisted=()
for path in "${bundled[@]}"; do
  [ -e "$path" ] || continue
  base=$(basename "$path")
  grep -Fxq -- "$base" <<<"$allowed" || unlisted+=("$base")
done

if [ ${#unlisted[@]} -gt 0 ]; then
  echo "error: the $system release would bundle libraries that are not on the allowlist in $0:" >&2
  printf '  %s\n' "${unlisted[@]}" >&2
  echo "Remove the dependency that pulls them in, or allowlist each one after a licence check." >&2
  exit 1
fi

echo "✓ Bundled $system libraries are all allowlisted"
