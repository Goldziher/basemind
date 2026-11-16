#!/usr/bin/env bash
# Clone the value-gate corpus (`corpus.tsv`) into DIR and verify every pin.
#
#   benchmarks/eval/fetch_corpus.sh DIR [NAME...]
#
# Each repository lands in DIR/NAME, detached at its pinned commit, and the sha256 of its
# `git ls-tree -r --full-tree HEAD` listing (blob ids included, so it pins content) must equal the
# committed one. `--print` prints the computed checksum and file count instead of checking them,
# for moving a pin. The clones are untrusted data: hooks are disabled and nothing in them is run.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
print=0
if [ "${1:-}" = "--print" ]; then
  print=1
  shift
fi
dir="${1:?usage: fetch_corpus.sh [--print] DIR [NAME...]}"
shift
mkdir -p "${dir}"
dir="$(cd "${dir}" && pwd -P)"
top="$(git -C "${here}" rev-parse --show-toplevel 2>/dev/null || true)"
if [ -n "${top}" ]; then
  case "${dir}/" in
  "$(cd "${top}" && pwd -P)/"*)
    echo "refusing to clone the corpus inside the repository (${dir})" >&2
    exit 2
    ;;
  esac
fi

git_safe() { git -c core.hooksPath=/dev/null "$@"; }

wanted() {
  [ "$#" -le 1 ] && return 0
  local name="$1"
  shift
  for n in "$@"; do [ "${n}" = "${name}" ] && return 0; done
  return 1
}

status=0
while IFS=$'\t' read -r name url commit sum files; do
  case "${name}" in '' | '#'*) continue ;; esac
  wanted "${name}" "$@" || continue
  dest="${dir}/${name}"
  if [ ! -d "${dest}/.git" ]; then
    GIT_LFS_SKIP_SMUDGE=1 git_safe clone --quiet --no-checkout --no-recurse-submodules "${url}" "${dest}"
  fi
  if ! git_safe -C "${dest}" cat-file -e "${commit}^{commit}" 2>/dev/null; then
    git_safe -C "${dest}" fetch --quiet origin "${commit}"
  fi
  GIT_LFS_SKIP_SMUDGE=1 git_safe -C "${dest}" -c advice.detachedHead=false checkout --quiet --force --detach "${commit}"
  git_safe -C "${dest}" clean -qfdx
  head="$(git -C "${dest}" rev-parse HEAD)"
  got_sum="$(git -C "${dest}" ls-tree -r --full-tree HEAD | shasum -a 256 | cut -d' ' -f1)"
  got_files="$(git -C "${dest}" ls-tree -r --full-tree --name-only HEAD | wc -l | tr -d ' ')"
  if [ "${print}" -eq 1 ]; then
    printf '%s\t%s\t%s\t%s\t%s\n' "${name}" "${url}" "${head}" "${got_sum}" "${got_files}"
    continue
  fi
  if [ "${head}" != "${commit}" ] || [ "${got_sum}" != "${sum}" ] || [ "${got_files}" != "${files}" ]; then
    echo "${name}: pin mismatch: HEAD ${head} sum ${got_sum} files ${got_files}" >&2
    echo "${name}: expected       ${commit} sum ${sum} files ${files}" >&2
    status=1
  else
    echo "${name}: ${commit} ok (${files} files)"
  fi
done <"${here}/corpus.tsv"
exit "${status}"
