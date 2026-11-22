#!/usr/bin/env bash
# Refuse to promote a draft whose archives do not match its checksums file.
#
# A rerun over a draft that already holds every asset replaces the archives one at a time and the
# checksums file last; a run that dies in between leaves new archives beside the old checksums,
# and every downloader then fails its checksum check. Name presence cannot see that, so this
# downloads each archive and the checksums file from the draft and verifies them.
#
# usage: verify-release-assets.sh <release_id> <version>
# Needs GITHUB_TOKEN and GITHUB_REPOSITORY (see release-asset.sh). RELEASE_ASSET_SH overrides the
# download helper, for tests.
set -euo pipefail

[ $# -eq 2 ] || {
  echo "usage: verify-release-assets.sh <release_id> <version>" >&2
  exit 2
}
release_id="$1"
version="$2"
helper="${RELEASE_ASSET_SH:-$(dirname "$0")/release-asset.sh}"
checksums="basemind_${version}_checksums.txt"
archives=(
  basemind-x86_64-unknown-linux-gnu.tar.gz
  basemind-aarch64-unknown-linux-gnu.tar.gz
  basemind-aarch64-apple-darwin.tar.gz
  basemind-x86_64-pc-windows-msvc.zip
)

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

bash "$helper" download "$release_id" "$checksums" "$work/$checksums"
for archive in "${archives[@]}"; do
  bash "$helper" download "$release_id" "$archive" "$work/$archive"
  # `-c` alone passes when the file lists nothing for an archive.
  if ! awk -v want="$archive" '{ n = $2; sub(/^\*/, "", n); if (n == want) found = 1 } END { exit !found }' \
    "$work/$checksums"; then
    echo "::error::$checksums does not list $archive" >&2
    exit 1
  fi
done

cd "$work"
if ! shasum -a 256 -c "$checksums" >&2; then
  echo "::error::release $release_id archives do not match $checksums; leaving it as a draft" >&2
  exit 1
fi
echo "verified ${#archives[@]} archives against $checksums"
