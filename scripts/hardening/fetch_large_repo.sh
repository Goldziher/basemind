#!/usr/bin/env bash
# Fetch one repository of the large-monorepo soak at its pinned commit.
#
#   scripts/hardening/fetch_large_repo.sh NAME DEST
#
# NAME is a row of large_repos.tsv. The fetch is scripts/harden_fetch.sh's, the one the harden
# harness pins its canary repositories with. The clone is untrusted data: hooks are disabled and
# nothing in it is run.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
name="${1:?usage: fetch_large_repo.sh NAME DEST}"
dest="${2:?usage: fetch_large_repo.sh NAME DEST}"

# shellcheck source=scripts/harden_fetch.sh
source "${here}/../harden_fetch.sh"

export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0=/dev/null
export GIT_LFS_SKIP_SMUDGE=1

while IFS=$'\t' read -r row url commit depth _paths; do
  case "${row}" in '' | '#'*) continue ;; esac
  [ "${row}" = "${name}" ] || continue
  harden_fetch "${name}" "${url}" "${commit}" "${depth}" "${dest}"
  exit 0
done <"${here}/large_repos.tsv"

echo "error: ${name} is not a row of large_repos.tsv" >&2
exit 2
