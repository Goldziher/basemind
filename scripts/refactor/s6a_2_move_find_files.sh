#!/usr/bin/env bash
# Engine unification S6a, step 2: move the `find_files` bodies and the filter refusals out of `src/mcp` into an engine op.
#
# Moves `src/mcp/helpers_files.rs` to `src/engine/ops/find_files.rs` and `src/mcp/helpers_filters.rs`
# to `src/engine/filters.rs` with `git mv`, then, in both:
# - `McpError::invalid_params(<msg>, None)` -> `EngineError::invalid_argument(<msg>)`;
# - `[super::helpers::]json_result(&<x>)` -> `Ok(<x>)`;
# - `state: &ServerState` -> `ws: &Workspace`, `state.shared.` -> `ws.`, `&state.shared` -> `ws`,
#   `state.store_read().await` -> `ws.store_read()`, `state.await_cache_ready().await` ->
#   `ws.wait_map_ready()`, `state` -> `ws` outside comments;
# - `[index_route::]blocking(|| <body>)` keeps its body: the op already runs on a blocking thread.
# Prints the count per shape and fails when a moved file still names rmcp or `McpError`, or
# when a constructor count does not match. The `CallToolResult` signatures, the result enum, the
# imports and the moved types are hand edits in the same commit. Run from the repository root.
set -euo pipefail

count() {
  local pattern=$1
  shift
  perl -0ne 'BEGIN { $n = 0 } $n++ while /'"$pattern"'/g; END { print $n }' "$@"
}

moves=(
  "src/mcp/helpers_files.rs src/engine/ops/find_files.rs"
  "src/mcp/helpers_filters.rs src/engine/filters.rs"
)
for pair in "${moves[@]}"; do
  read -r from to <<<"$pair"
  if [ ! -f "$from" ]; then
    echo "$from is gone: already moved?" >&2
    exit 1
  fi
done
to=()
for pair in "${moves[@]}"; do
  read -r from dest <<<"$pair"
  git mv "$from" "$dest"
  to+=("$dest")
done

invalid=$(count 'McpError::invalid_params\(' "${to[@]}")
json=$(count '\bjson_result\(&' "${to[@]}")
blocking=$(count '\bblocking\(\|\|' "${to[@]}")
awaits=$(count 'store_read\(\)\.await|await_cache_ready\(\)\.await' "${to[@]}")
perl -0pi -e '
  s{McpError::invalid_params(\((?:[^()]++|(?1))*\))}{
    my $args = $1;
    $args =~ s/,\s*None\s*,?\s*\)\z/)/s;
    "EngineError::invalid_argument$args"
  }gse;
  s/\b(?:super::helpers::)?json_result\(&/Ok(/g;
  s/\bsuper::index_route::blocking\(\|\| ((?:[^()]++|\((?1)\))*)\)/$1/g;
  s/\bblocking\(\|\| ((?:[^()]++|\((?1)\))*)\)/$1/g;
  s/state\.store_read\(\)\.await/ws.store_read()/g;
  s/state\.await_cache_ready\(\)\.await/ws.wait_map_ready()/g;
  s/^use rmcp::ErrorData as McpError;\n/use crate::engine::error::EngineError;\n/m;
  s/^use rmcp::model::CallToolResult;\n//m;
  s/\bMcpError\b/EngineError/g;
  s/\bstate: &ServerState\b/ws: &Workspace/g;
  s/\bstate\.shared\./ws./g;
  s/&state\.shared\b/ws/g;
' "${to[@]}"
perl -pi -e 's/\bstate\b/ws/g unless m{^\s*//}' "${to[@]}"
argument=$(count 'EngineError::invalid_argument\(' "${to[@]}")
rmcp_left=$(count '\brmcp\b|McpError' "${to[@]}")
echo "moved ${#to[@]} files; retyped $invalid invalid_params -> $argument invalid_argument; rewrote $json" \
  "json_result sites, $blocking blocking sites, $awaits awaited reads; $rmcp_left rmcp / McpError left"
test "$rmcp_left" -eq 0
test "$invalid" -eq "$argument"
