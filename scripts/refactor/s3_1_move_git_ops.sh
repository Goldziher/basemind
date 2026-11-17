#!/usr/bin/env bash
# Engine unification S3, step 1: move the git tool bodies out of `src/mcp` into `src/engine/git`,
# with their types and the symbol picker, and retype their errors as `EngineError`.
#
# Expects `src/engine/git/common.rs` to hold the git part of `src/mcp/helpers.rs`, cut out by hand
# in the same commit. Moves the rest with `git mv`, then over every moved file:
# - `McpError::invalid_params(<msg>, None)` -> `EngineError::invalid_argument(<msg>)`,
#   `internal_error` -> `internal`, `invalid_request` -> `unavailable` (same MCP codes, same text);
# - `json_result(&<x>)` -> `Ok(<x>)`: an operation returns its typed answer;
# - `state: &ServerState` -> `ws: &Workspace`, `state.shared.` -> `ws.`, `state` -> `ws` outside comments;
# - `rmcp::schemars` -> the direct `schemars` dependency.
# Prints the count per shape and fails when a moved body still names rmcp or `McpError`, or when a
# constructor count does not match. The `CallToolResult` signatures, the `ServerState` imports and
# the moved tests are hand edits in the same commit (the result enums, the `Workspace` methods).
# Run from the repository root.
set -euo pipefail

# Occurrences of a multi-line Perl pattern across the given files.
count() {
  local pattern=$1
  shift
  perl -0ne 'BEGIN { $n = 0 } $n++ while /'"$pattern"'/g; END { print $n }' "$@"
}

test -f src/engine/git/common.rs || {
  echo "src/engine/git/common.rs is missing: cut the git part of src/mcp/helpers.rs into it first" >&2
  exit 1
}

moves=(
  "src/mcp/helpers_git.rs src/engine/git/log.rs"
  "src/mcp/helpers_git_file.rs src/engine/git/file.rs"
  "src/mcp/helpers_git_by_path.rs src/engine/git/by_path.rs"
  "src/mcp/helpers_git_by_path_tests.rs src/engine/git/by_path_tests.rs"
  "src/mcp/helpers_git_show.rs src/engine/git/show.rs"
  "src/mcp/helpers_fingerprint.rs src/engine/git/fingerprint.rs"
  "src/mcp/helpers_pick.rs src/engine/pick.rs"
  "src/mcp/types_git.rs src/engine/types/git.rs"
)
for pair in "${moves[@]}"; do
  read -r from to <<<"$pair"
  if [ ! -f "$from" ]; then
    echo "$from is gone: already moved?" >&2
    exit 1
  fi
done
for pair in "${moves[@]}"; do
  read -r from to <<<"$pair"
  git mv "$from" "$to"
done

files=(
  src/engine/git/common.rs src/engine/git/log.rs src/engine/git/file.rs src/engine/git/by_path.rs
  src/engine/git/by_path_tests.rs src/engine/git/show.rs src/engine/git/fingerprint.rs src/engine/pick.rs
  src/engine/types/git.rs
)

invalid=$(count 'McpError::invalid_params\(' "${files[@]}")
internal=$(count 'McpError::internal_error\(' "${files[@]}")
request=$(count 'McpError::invalid_request\(' "${files[@]}")
json=$(count '\bjson_result\(&' "${files[@]}")
perl -0pi -e '
  BEGIN { %to = (invalid_params => "invalid_argument", internal_error => "internal", invalid_request => "unavailable") }
  s{McpError::(invalid_params|internal_error|invalid_request)(\((?:[^()]++|(?2))*\))}{
    my ($kind, $args) = ($1, $2);
    $args =~ s/,\s*None\s*,?\s*\)\z/)/s;
    "EngineError::$to{$kind}$args"
  }gse;
  s/\bjson_result\(&/Ok(/g;
  s/^use rmcp::ErrorData as McpError;\n/use crate::engine::error::EngineError;\n/m;
  s/^use rmcp::model::CallToolResult;\n//m;
  s/^use rmcp::schemars;\n//m;
  s/\brmcp::schemars::/schemars::/g;
  s/\bMcpError\b/EngineError/g;
  s/\bstate: &ServerState\b/ws: &Workspace/g;
  s/\bstate: &\x27a ServerState\b/ws: &\x27a Workspace/g;
  s/\bstate\.shared\./ws./g;
  s/&state\.shared\b/ws/g;
' "${files[@]}"
# The variable, not the word in a comment ("the state the window is compared against").
perl -pi -e 's/\bstate\b/ws/g unless m{^\s*//}' "${files[@]}"
argument=$(count 'EngineError::invalid_argument\(' "${files[@]}")
internals=$(count 'EngineError::internal\(' "${files[@]}")
unavailable=$(count 'EngineError::unavailable\(' "${files[@]}")
oks=$(count '\bOk\(' "${files[@]}")

# The moved tests build their workspace by hand; the bodies name no rmcp type after this.
bodies=()
for file in "${files[@]}"; do
  [ "$file" = src/engine/git/by_path_tests.rs ] || bodies+=("$file")
done
rmcp_left=$(count '\brmcp\b|McpError' "${bodies[@]}")
hand=$(count 'CallToolResult|ServerState|json_result' "${bodies[@]}")
echo "moved ${#moves[@]} files; retyped $invalid invalid_params -> $argument invalid_argument," \
  "$internal internal_error -> $internals internal, $request invalid_request -> $unavailable unavailable;" \
  "rewrote $json json_result sites ($oks Ok( in all); $rmcp_left rmcp / McpError mentions left;" \
  "$hand CallToolResult / ServerState / json_result mentions (signatures, imports) left for the hand edits"
test "$rmcp_left" -eq 0
test "$invalid" -eq "$argument"
test "$internal" -eq "$internals"
test "$request" -eq "$unavailable"
test "$json" -gt 0
