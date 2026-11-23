#!/usr/bin/env bash
# The pinned-repository fetch shared by scripts/harden.sh and the large-monorepo soak
# (scripts/hardening/fetch_large_repo.sh). Source it; it defines `harden_fetch` and runs nothing.
#
#   harden_fetch NAME URL COMMIT DEPTH DEST
#
# DEST ends up detached at exactly COMMIT, or the call fails. An empty DEPTH fetches the full
# history. A DEST already at COMMIT is reused.

harden_fetch() {
  local name="$1" url="$2" commit="$3" depth="$4" dest="$5"
  if [ "$(git -C "${dest}" rev-parse HEAD 2>/dev/null || true)" = "${commit}" ]; then
    echo "==> reusing the clone at ${dest} (${commit})"
    return 0
  fi
  echo "==> fetching ${name}: ${url} at ${commit}${depth:+ (depth ${depth})} → ${dest}"
  rm -rf "${dest}"
  git init -q "${dest}"
  git -C "${dest}" remote add origin "${url}"
  git -C "${dest}" fetch -q ${depth:+--depth="${depth}"} origin "${commit}"
  git -C "${dest}" checkout -q --detach FETCH_HEAD
  [ "$(git -C "${dest}" rev-parse HEAD)" = "${commit}" ] || {
    echo "error: ${dest} is not at the pinned ${commit}" >&2
    return 1
  }
}
