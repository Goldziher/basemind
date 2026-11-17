#!/usr/bin/env bash
# Engine unification S1, step 2: rename `SharedReadStack` to `engine::Workspace`.
#
# Re-runnable: prints the number of rewritten sites and fails when there is nothing to rewrite.
# Run from the repository root after s1_1_move_engine_modules.sh.
set -euo pipefail

files=$(git grep -l -w SharedReadStack -- src tests || true)
before=$(git grep -ow SharedReadStack -- src tests | wc -l | tr -d ' ')
if [ "$before" -eq 0 ]; then
  echo "nothing to rewrite: no SharedReadStack left" >&2
  exit 1
fi
# shellcheck disable=SC2086
perl -pi -e 's/\bSharedReadStack\b/Workspace/g' $files
after=$(git grep -ow SharedReadStack -- src tests | wc -l | tr -d ' ')
echo "rewrote $before SharedReadStack sites in $(echo "$files" | wc -l | tr -d ' ') files; $after left"
test "$after" -eq 0

# Import order and blank lines as rustfmt wants them.
cargo fmt
