#!/usr/bin/env bash
# Engine unification S2, step 1: move the read-side support modules the `code_outline` operation
# needs out of `src/mcp` into `src/engine`, and retype their errors as `EngineError`.
#
# Moves `cursor`, `freshness`, `budget` and `tokens` with `git mv`, rewrites each moved
# `McpError::invalid_params(<msg>, None)` to `EngineError::invalid_argument(<msg>)`, points the
# schema derives at the direct `schemars` dependency, and rewrites the call sites that passed a
# `ServerState` where the moved code now takes the `Workspace`. Prints the count per shape and fails
# when a moved file still names rmcp. Run from the repository root; the hand edits that follow
# (the `mcp` re-exports, `engine/files.rs`, the moved tests) are in the same commit.
set -euo pipefail

# Occurrences of a multi-line Perl pattern across the given files.
count() {
  local pattern=$1
  shift
  perl -0ne 'BEGIN { $n = 0 } $n++ while /'"$pattern"'/g; END { print $n }' "$@"
}

for module in cursor freshness budget tokens; do
  if [ ! -f "src/mcp/$module.rs" ]; then
    echo "src/mcp/$module.rs is gone: already moved?" >&2
    exit 1
  fi
done
for module in cursor freshness budget tokens; do
  git mv "src/mcp/$module.rs" "src/engine/$module.rs"
done

moved=(src/engine/cursor.rs src/engine/freshness.rs src/engine/budget.rs src/engine/tokens.rs)
invalid=$(count 'McpError::invalid_params\(' "${moved[@]}")
perl -0pi -e '
  s{McpError::invalid_params(\((?:[^()]++|(?1))*\))}{
    my $args = $1;
    $args =~ s/,\s*None\s*,?\s*\)\z/)/s;
    "EngineError::invalid_argument$args"
  }gse;
  s/^use rmcp::ErrorData as McpError;\n/use crate::engine::error::EngineError;\n/m;
  s/^use rmcp::schemars;\n//m;
  s/\bMcpError\b/EngineError/g;
' "${moved[@]}"
retyped=$(count 'EngineError::invalid_argument\(' "${moved[@]}")

# Callers that handed the moved code a `ServerState` now hand it the shared `Workspace`.
callers=$(git grep -lE 'IndexedFiles::of\(state\)|freshness::reparse\(state,' -- src/mcp || true)
of_sites=0
reparse_sites=0
if [ -n "$callers" ]; then
  # shellcheck disable=SC2086
  of_sites=$(count 'IndexedFiles::of\(state\)' $callers)
  # shellcheck disable=SC2086
  reparse_sites=$(count 'freshness::reparse\(state,' $callers)
  # shellcheck disable=SC2086
  perl -pi -e 's/IndexedFiles::of\(state\)/IndexedFiles::of(&state.shared)/g; s/freshness::reparse\(state,/freshness::reparse(&state.shared,/g' $callers
fi

left=$(count '\brmcp\b|McpError' "${moved[@]}")
echo "moved ${#moved[@]} modules; retyped $invalid invalid_params sites into $retyped invalid_argument sites;" \
  "rewrote $of_sites IndexedFiles::of and $reparse_sites reparse call sites; $left rmcp mentions left"
test "$left" -eq 0
test "$invalid" -eq "$retyped"
test "$invalid" -gt 0
