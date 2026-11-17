#!/usr/bin/env bash
# Engine unification S1, step 1: move the rmcp-free read-stack modules from `src/mcp/` to
# `src/engine/`, keeping re-exports in `src/mcp/mod.rs` so no other file changes.
#
# Re-runnable on a fresh checkout or after a rebase: it refuses to run once the move is done.
# Run from the repository root.
set -euo pipefail

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if [ ! -e src/mcp/shared_state.rs ]; then
  echo "already applied: src/mcp/shared_state.rs is gone" >&2
  exit 1
fi

mkdir -p src/engine

git mv src/mcp/shared_state.rs src/engine/workspace.rs
for f in l1_cache outline_cache ranked_cache term_index term_index_tests map_cache_budget_tests map_cache_delta_tests; do
  git mv "src/mcp/$f.rs" "src/engine/$f.rs"
done

# MapCache, doc_paths_of and the two `#[path]` test modules are the tail of state.rs, from the
# struct to the end of the file.
perl -0ne 'print $1 if /^(pub\(crate\) struct MapCache \{.*)\z/ms' src/mcp/state.rs >"$tmp"/s1_map_cache_body.rs
perl -0pi -e 's/^pub\(crate\) struct MapCache \{.*\z//ms' src/mcp/state.rs
test -s "$tmp"/s1_map_cache_body.rs

# Lifecycle, its classifier and the warm-wait cap move beside the workspace.
perl -0ne 'print $1 if /^(\/\/\/ Upper bound a cache-reading tool waits.*?\n\}\n\nimpl Lifecycle \{.*?\n\}\n)/ms' src/mcp/state.rs >"$tmp"/s1_lifecycle.rs
perl -0pi -e 's/^\/\/\/ Upper bound a cache-reading tool waits.*?\n\}\n\nimpl Lifecycle \{.*?\n\}\n\n//ms' src/mcp/state.rs
test -s "$tmp"/s1_lifecycle.rs

# OutlineEntry moves into the cache that holds it.
perl -0ne 'print $1 if /^(pub\(crate\) struct OutlineEntry \{.*?\n\}\n)/ms' src/mcp/mod.rs >"$tmp"/s1_outline_entry.rs
perl -0pi -e 's/^pub\(crate\) struct OutlineEntry \{.*?\n\}\n\npub\(crate\) use outline_cache::OutlineCache;\n/pub(crate) use crate::engine::outline_cache::{OutlineCache, OutlineEntry};\n/ms' src/mcp/mod.rs
test -s "$tmp"/s1_outline_entry.rs

{
  cat <<'EOF'
//! The in-RAM map over a workspace's index: [`MapCache`], the snapshot every cache-reading tool
//! loads once per call.

use std::sync::Arc;

use crate::engine::l1_cache;
use crate::extract::FileMapL1;
use crate::mcp::{helpers_calls, helpers_impls};
use crate::store::Store;

EOF
  cat "$tmp"/s1_map_cache_body.rs
} >src/engine/map_cache.rs

LIFECYCLE="$(cat "$tmp/s1_lifecycle.rs")" perl -0pi -e 's/\n#\[cfg\(test\)\]\nmod tests \{/\n$ENV{LIFECYCLE}\n\n#[cfg(test)]\nmod tests {/' src/engine/workspace.rs
grep -q "pub(crate) enum Lifecycle" src/engine/workspace.rs

perl -0pi -e 's/(use super::OutlineEntry;\n)/use crate::extract::FileMapL1;\n/' src/engine/outline_cache.rs
ENTRY="/// A parsed outline and the source it was parsed from.
$(cat "$tmp/s1_outline_entry.rs")" perl -0pi -e 's/\n#\[cfg\(test\)\]\nmod tests \{/\n$ENV{ENTRY}\n\n#[cfg(test)]\nmod tests {/' src/engine/outline_cache.rs
grep -q "pub(crate) struct OutlineEntry" src/engine/outline_cache.rs

cat >src/engine/mod.rs <<'EOF'
//! The synchronous engine: one [`Workspace`] per served `(root, view)` and the caches it reads
//! through. Nothing here names rmcp, clap or tokio types (ADR-0039); the MCP adapter and the CLI
//! reach it from `src/mcp`.

pub(crate) mod l1_cache;
pub(crate) mod map_cache;
pub(crate) mod outline_cache;
pub(crate) mod ranked_cache;
pub(crate) mod term_index;
pub(crate) mod workspace;

pub(crate) use map_cache::MapCache;
pub(crate) use workspace::{Lifecycle, SharedReadStack};
EOF

# lib.rs: declare the module beside `embeddings`.
perl -0pi -e 's/(pub mod embeddings;\n)/$1pub(crate) mod engine;\n/' src/lib.rs

# Paths inside the moved files: engine siblings by `crate::engine`, the rest of mcp by `crate::mcp`.
moved=(src/engine/workspace.rs src/engine/map_cache.rs src/engine/l1_cache.rs src/engine/outline_cache.rs src/engine/ranked_cache.rs src/engine/term_index.rs)
n_engine=$(grep -oE 'super::(l1_cache|outline_cache|ranked_cache|term_index|MapCache|OutlineCache)\b' "${moved[@]}" | wc -l)
perl -pi -e 's/\bsuper::(l1_cache|outline_cache|ranked_cache|term_index)\b/crate::engine::$1/g; s/\bsuper::(MapCache|OutlineCache)\b/crate::engine::$1/g' "${moved[@]}"
n_mcp=$(grep -ohE 'super::[a-zA-Z{]' "${moved[@]}" | grep -vc 'super::\*' || true)
perl -pi -e 's/\bsuper::(?!\*)/crate::mcp::/g' "${moved[@]}"
n_vis=$(grep -o 'pub(super)' "${moved[@]}" | wc -l || true)
perl -pi -e 's/\bpub\(super\)/pub(crate)/g' "${moved[@]}"
echo "rewrote: $n_engine engine paths, $n_mcp mcp paths, $n_vis pub(super) items"
test "$n_engine" -gt 0

# The workspace file's own `use` of moved items.
perl -0pi -e 's/use crate::mcp::\{MapCache, OutlineCache, ServerOptions, telemetry\};/use crate::engine::{MapCache, outline_cache::OutlineCache};\nuse crate::mcp::{ServerOptions, telemetry};/' src/engine/workspace.rs
perl -0pi -e 's/use super::\{SharedReadStack, helpers_calls, helpers_impls, l1_cache, types\};\nuse crate::extract::FileMapL1;\nuse crate::store::Store;\n/use super::{SharedReadStack, l1_cache, types};\nuse crate::engine::{Lifecycle, MapCache, workspace::CACHE_WARM_WAIT_CAP};\n/' src/mcp/state.rs

# mcp/mod.rs: the moved modules become re-exports, and the modules the engine still reaches into
# (interim layering edges, ADR-0039) become crate-visible.
perl -pi -e 's/^mod (l1_cache|ranked_cache);\n/pub(crate) use crate::engine::$1;\n/; s/^mod (outline_cache|term_index);\n//; s/^mod shared_state;\n//' src/mcp/mod.rs
perl -pi -e 's/^mod (helpers_calls|helpers_impls|helpers_files|identity|lance_cell|memory_vectors|telemetry);/pub(crate) mod $1;/' src/mcp/mod.rs
perl -pi -e 's/^pub\(crate\) use shared_state::SharedReadStack;/pub(crate) use crate::engine::SharedReadStack;/; s/^pub\(crate\) use state::\{Lifecycle, MapCache, ServerState\};/pub(crate) use crate::engine::{Lifecycle, MapCache};\npub(crate) use state::ServerState;/; s/\bshared_state::boot_plan\b/crate::engine::workspace::boot_plan/g' src/mcp/mod.rs

# Items the engine reaches in mcp until their owners move (interim edges, ADR-0039).
perl -pi -e 's/^pub\(super\) struct TrackedCache\b/pub(crate) struct TrackedCache/' src/mcp/helpers_files.rs
perl -pi -e 's/^pub\(super\) fn resolve_agent_id\b/pub(crate) fn resolve_agent_id/' src/mcp/identity.rs
perl -0pi -e 's/use crate::extract::FileMapL1;\n//' src/mcp/mod.rs

# Cargo `include` excludes follow the test files.
perl -pi -e 's{"!/src/mcp/(map_cache_budget_tests|term_index_tests)\.rs"}{"!/src/engine/$1.rs"}' Cargo.toml

echo "step 1 applied"

# Import order and blank lines as rustfmt wants them.
cargo fmt
