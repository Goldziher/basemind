#!/usr/bin/env bash
# Release-record helpers for publish.yaml: resolve a tag to exactly one release id, then move
# assets on and off that id rather than on the tag.
#
# Both halves exist because of the v0.27.0 publish run. Its draft was promoted by hand while the
# upload matrix was still going, which left two release records sharing one tag; every later
# `gh release upload "$tag"` followed the tag to the newly-promoted record while the earlier
# uploads stayed on the draft. The run finished with five archives split across two objects, no
# checksums file, and the PyPI/npm/crates/Homebrew jobs all skipped. A release id names exactly
# one object, and `resolve` refuses to continue while the tag is ambiguous.
#
# `gh release upload/download` cannot be pointed at an id — they resolve by tag — hence the direct
# REST calls here.
#
# Needs GITHUB_TOKEN and GITHUB_REPOSITORY. Uses `gh api -q` rather than jq throughout: this also
# runs inside the manylinux build containers, which ship no jq.
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage:
  release-asset.sh resolve  <tag> [require_draft] [expected_id]   # prints the release id
  release-asset.sh upload   <release_id> <file> <content_type>
  release-asset.sh download <release_id> <asset_name> <output_path>
EOF
  exit 2
}

# Asset id of <asset_name> on release <release_id>, or empty. Filters in awk rather than
# interpolating the name into a jq expression.
asset_id() {
  gh api "repos/$GITHUB_REPOSITORY/releases/$1/assets?per_page=100" \
    -q '.[] | "\(.id) \(.name)"' | awk -v want="$2" '$2 == want { print $1; exit }'
}

# Print the one release id carrying <tag>, refusing to continue if the count is not exactly one.
#
# `require_draft` (default true) additionally demands the record still be a draft, so a hand
# promotion is caught at the step that would have scattered assets rather than three jobs later.
# `expected_id` is the id this run pinned at creation; empty means "no expectation", which is the
# normal state for jobs that run when `create_release` was skipped because the release was
# already complete.
cmd_resolve() {
  local tag="$1" require_draft="${2:-true}" expected_id="${3:-}" ids count state
  ids="$(gh api "repos/$GITHUB_REPOSITORY/releases?per_page=100" \
    -q '.[] | "\(.id) \(.tag_name)"' | awk -v want="$tag" '$2 == want { print $1 }')"
  count="$(printf '%s' "$ids" | grep -c . || true)"

  if [ "$count" -gt 1 ]; then
    {
      echo "::error::tag $tag resolves to $count release records: $(printf '%s' "$ids" | tr '\n' ' ')"
      echo "Assets addressed by tag would scatter across them — this is the v0.27.0 failure."
      echo "Do NOT publish the draft by hand; the finalize job is what promotes it."
      echo "Delete the extra record(s), keep the tag, and re-run this workflow."
    } >&2
    exit 1
  fi
  if [ "$count" -eq 0 ]; then
    echo "::error::tag $tag has no release record" >&2
    exit 1
  fi
  if [ -n "$expected_id" ] && [ "$ids" != "$expected_id" ]; then
    {
      echo "::error::tag $tag now points at release $ids, not the $expected_id this run pinned"
      echo "The original record was deleted and recreated mid-run. Re-run the workflow."
    } >&2
    exit 1
  fi
  if [ "$require_draft" = "true" ]; then
    state="$(gh api "repos/$GITHUB_REPOSITORY/releases/$ids" -q '.draft')"
    if [ "$state" != "true" ]; then
      {
        echo "::error::release $ids (tag $tag) is no longer a draft"
        echo "It was published before its assets were complete. Do NOT publish by hand."
        echo "To recover: delete release $ids (\`gh release delete $tag --cleanup-tag=false\`),"
        echo "leave the git tag in place, and re-run this workflow — it rebuilds the draft from scratch."
      } >&2
      exit 1
    fi
  fi

  printf '%s\n' "$ids"
}

cmd_upload() {
  local release_id="$1" file="$2" content_type="$3" name existing
  if [ ! -f "$file" ]; then
    echo "::error::asset not found: $file" >&2
    exit 1
  fi
  name="$(basename "$file")"

  # Replace rather than fail. Re-running a partially-failed publish finds its own earlier uploads
  # already attached, and the asset POST answers 422 already_exists — so without this, recovering
  # from exactly the situation this pipeline got into would be impossible. This is what
  # `gh release upload --clobber` did before the move to id addressing.
  existing="$(asset_id "$release_id" "$name")"
  if [ -n "$existing" ]; then
    echo "replacing existing asset $name (id $existing)"
    gh api -X DELETE "repos/$GITHUB_REPOSITORY/releases/assets/$existing" >/dev/null
  fi

  echo "uploading $name to release $release_id"
  # curl, not `gh api --input`: this is a raw binary body, and --data-binary is the unambiguous
  # way to send one. POSTs to uploads.github.com are not redirected, so the Authorization header
  # has no other host to leak to.
  curl -fsS -X POST \
    -H "Authorization: Bearer $GITHUB_TOKEN" \
    -H "Accept: application/vnd.github+json" \
    -H "Content-Type: $content_type" \
    --data-binary @"$file" \
    "https://uploads.github.com/repos/$GITHUB_REPOSITORY/releases/$release_id/assets?name=$name" \
    >/dev/null
  echo "uploaded $name"
}

cmd_download() {
  local release_id="$1" name="$2" out="$3" id
  id="$(asset_id "$release_id" "$name")"
  if [ -z "$id" ]; then
    echo "::error::asset $name not found on release $release_id" >&2
    exit 1
  fi

  # The asset API with an octet-stream Accept, NOT browser_download_url: for as long as the
  # release is a draft — which is most of this pipeline's run — the browser URL is not fetchable,
  # token or no token. `gh api` rather than curl because the asset endpoint redirects to storage
  # and curl would forward the Authorization header to that host.
  gh api -H "Accept: application/octet-stream" \
    "repos/$GITHUB_REPOSITORY/releases/assets/$id" >"$out"
  echo "downloaded $name to $out"
}

[ $# -ge 1 ] || usage
case "$1" in
resolve)
  [ $# -ge 2 ] && [ $# -le 4 ] || usage
  cmd_resolve "$2" "${3:-true}" "${4:-}"
  ;;
upload)
  [ $# -eq 4 ] || usage
  cmd_upload "$2" "$3" "$4"
  ;;
download)
  [ $# -eq 4 ] || usage
  cmd_download "$2" "$3" "$4"
  ;;
*) usage ;;
esac
