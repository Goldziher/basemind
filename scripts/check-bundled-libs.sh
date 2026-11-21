#!/usr/bin/env bash
# Fails when a release archive's staging directory ships x265 or bundles a native library that is not
# allowlisted, or when an allowlisted library has no licence notice in the repository. When every
# check passes, copies each bundled library's notice into <staging-dir>/licenses/<library>.txt.
#
# Usage: check-bundled-libs.sh <linux|macos|windows> <staging-dir>
#
# Linux and macOS bundle libraries under <staging-dir>/lib/; Windows puts DLLs next to the binary.
set -euo pipefail

# One "<basename> <notice>" pair per line. Every entry needs a licence check, its licence text in
# licenses/<notice> and an ATTRIBUTIONS.md entry naming that file; this script fails for any entry, on
# any platform, that lacks either. Basenames are exact, so a soname bump fails the build and gets the
# same review. Libraries the loader takes from the host (glibc, libgcc_s, /usr/lib, /System) are
# never bundled and so never listed.
ALLOWED_LINUX='
libstdc++.so.6 libstdc++.so.6.txt
libheif.so.1 libheif.txt
libde265.so.0 libde265.txt
'
ALLOWED_MACOS='
libheif.1.dylib libheif.txt
libde265.0.dylib libde265.txt
'
ALLOWED_WINDOWS='
heif.dll libheif.txt
libde265.dll libde265.txt
'

# x265 is a GPL HEVC encoder; basemind only decodes HEIC. A library named for it, the banner format
# string in x265's encoder, or the name libheif gives its x265 plugin each mean it would ship, linked
# dynamically or statically.
X265_MARKERS=('x265 (build' 'x265 HEVC encoder')

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
  binary="$staging/basemind"
  ;;
macos)
  allowed="$ALLOWED_MACOS"
  bundled=("$staging"/lib/*)
  binary="$staging/basemind"
  ;;
windows)
  allowed="$ALLOWED_WINDOWS"
  # The Windows loader matches DLL names without regard to case.
  shopt -s nocaseglob
  bundled=("$staging"/*.dll)
  shopt -u nocaseglob
  binary="$staging/basemind.exe"
  ;;
*)
  echo "Unknown system: $system (expected linux, macos or windows)" >&2
  exit 2
  ;;
esac

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
unnoticed=()
while IFS= read -r notice; do
  [ -n "$notice" ] || continue
  [ -f "$root/licenses/$notice" ] || unnoticed+=("licenses/$notice is missing")
  grep -Fq -- "licenses/$notice" "$root/ATTRIBUTIONS.md" 2>/dev/null ||
    unnoticed+=("ATTRIBUTIONS.md does not name licenses/$notice")
done < <(printf '%s\n' "$ALLOWED_LINUX" "$ALLOWED_MACOS" "$ALLOWED_WINDOWS" | awk 'NF { print $2 }' | sort -u)

if [ ${#unnoticed[@]} -gt 0 ]; then
  echo "error: libraries allowlisted in $0 lack a licence notice:" >&2
  printf '  %s\n' "${unnoticed[@]}" >&2
  echo "Add each library's licence text under licenses/ and name it in ATTRIBUTIONS.md." >&2
  exit 1
fi

x265=()
for path in "${bundled[@]}"; do
  [ -e "$path" ] || continue
  case "$(basename "$path" | tr '[:upper:]' '[:lower:]')" in
  *x265*) x265+=("${path#"$staging"/} (an x265 library)") ;;
  esac
done
for path in "$binary" "${bundled[@]}"; do
  [ -f "$path" ] || continue
  for marker in "${X265_MARKERS[@]}"; do
    if LC_ALL=C grep -aFq -- "$marker" "$path"; then
      x265+=("${path#"$staging"/} (contains \"$marker\")")
    fi
  done
done

if [ ${#x265[@]} -gt 0 ]; then
  echo "error: the $system release would ship x265, a GPL HEVC encoder basemind must not distribute:" >&2
  printf '  %s\n' "${x265[@]}" >&2
  echo "Build libheif without x265 (scripts/build-libheif.sh, .github/actions/install-libheif)." >&2
  exit 1
fi

# The notice of an allowlisted basename, or nothing for an unlisted one.
notice_for() {
  awk -v lib="$1" '$1 == lib { print $2 }' <<<"$allowed"
}

unlisted=()
for path in "${bundled[@]}"; do
  [ -e "$path" ] || continue
  base=$(basename "$path")
  [ -n "$(notice_for "$base")" ] || unlisted+=("$base")
done

if [ ${#unlisted[@]} -gt 0 ]; then
  echo "error: the $system release would bundle libraries that are not on the allowlist in $0:" >&2
  printf '  %s\n' "${unlisted[@]}" >&2
  echo "Remove the dependency that pulls them in, or allowlist each one after a licence check." >&2
  exit 1
fi

for path in "${bundled[@]}"; do
  [ -e "$path" ] || continue
  base=$(basename "$path")
  mkdir -p "$staging/licenses"
  cp "$root/licenses/$(notice_for "$base")" "$staging/licenses/$base.txt"
done

echo "✓ Bundled $system libraries are all allowlisted"
