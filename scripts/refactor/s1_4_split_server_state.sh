#!/usr/bin/env bash
# Engine unification S1, step 4: the host-specific fields (telemetry writer, daemon topology) move
# from `engine::Workspace` to `mcp::ServerState`. Rewrites the field accesses; the struct and
# constructor changes are the hand-written half of the commit. Prints the count and fails when
# nothing was rewritten or an access is left. Run from the repository root.
set -euo pipefail

pattern='\.shared\s*\.(telemetry|host|daemon_writer|memory_is_framed)\b'
# Multi-line: a chain may put `.shared` and the field on lines of their own.
# shellcheck disable=SC2016
files=$(git ls-files 'src/mcp/*.rs' 'src/comms/*.rs' | xargs perl -0ne 'print "$ARGV\n" if /'"$pattern"'/; close ARGV' | sort -u)
if [ -z "$files" ]; then
  echo "nothing to rewrite: no shared.{telemetry,host,daemon_writer} access left" >&2
  exit 1
fi
# shellcheck disable=SC2086
count() { perl -0ne 'BEGIN { $n = 0 } $n++ while /'"$pattern"'/g; END { print $n }' $files; }
before=$(count)
# shellcheck disable=SC2086
perl -0pi -e 's/\.shared(\s*)\.(telemetry|host|daemon_writer|memory_is_framed)\b/$1.$2/g' $files
after=$(count)
echo "rewrote $before field accesses in $(echo "$files" | wc -l | tr -d ' ') files; $after left"
test "$before" -gt 0
test "$after" -eq 0

cargo fmt
