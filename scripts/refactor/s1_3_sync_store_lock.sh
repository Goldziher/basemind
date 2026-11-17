#!/usr/bin/env bash
# Engine unification S1, step 3: point every store-lock site at the synchronous `Workspace`.
#
# An async caller goes through the `ServerState::store_read`/`store_write` seam (a free lock is
# taken at once, a held one is waited for inside `index_route::blocking`); a caller already on a
# blocking thread takes the `Workspace` lock directly. Prints the count per shape and fails when a
# tokio-lock shape is left. Run from the repository root after s1_2_rename_workspace.sh.
set -euo pipefail

# Occurrences of a multi-line Perl pattern across `$files`.
# shellcheck disable=SC2086
count() { perl -0ne 'BEGIN { $n = 0 } $n++ while /'"$1"'/g; END { print $n }' $files; }

files=$(git grep -lP 'shared\s*\.\s*store\s*\.\s*(read|write|blocking_read|blocking_write|try_read)\(\)' -- src || true)
if [ -z "$files" ]; then
  echo "nothing to rewrite: no tokio store-lock site left" >&2
  exit 1
fi
awaits=$(count 'shared\.store\.(read|write)\(\)\s*\.await')
blocking=$(count 'shared\.store\.blocking_(read|write)\(\)')
tries=$(count 'shared\s*\.store\s*\.try_read\(\)')

# shellcheck disable=SC2086
perl -0pi -e '
  s/\b(state|self)\.shared\.store\.read\(\)\s*\.await/$1.store_read().await/g;
  s/\b(state|self)\.shared\.store\.write\(\)\s*\.await/$1.store_write().await/g;
  s/\.shared\.store\.blocking_read\(\)/.shared.store_read()/g;
  s/\.shared\.store\.blocking_write\(\)/.shared.store_write()/g;
  s/\.shared(\s*)\.store\s*\.try_read\(\)/.shared$1.try_store_read()/g;
' $files

left=$(count 'shared\s*\.\s*store\s*\.\s*(read|write|blocking_read|blocking_write|try_read)\(\)')
echo "rewrote $awaits .await sites, $blocking blocking_* sites, $tries try_read sites; $left left"
test "$left" -eq 0
test "$awaits" -gt 0

cargo fmt
