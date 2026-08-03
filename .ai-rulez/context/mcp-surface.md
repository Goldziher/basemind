---
priority: high
---

# MCP Surface

`basemind serve` exposes a stdio MCP server (`rmcp`). The live contract is `tests/mcp_smoke.rs` — read it before changing any tool's response shape.

## Code-map tools

| Tool | Purpose |
|---|---|
| `outline` | Full per-file structure: symbols + line/col + signatures + imports. `l2: true` includes calls + docs. |
| `search_symbols` | Substring lookup across every indexed file, with optional kind filter. In-RAM `memmem`. |
| `find_references` | Call sites of any callee whose identifier matches `name`. Backed by Fjall `calls_by_callee`. No scope resolution; `Foo::bar()` and `bar()` both match `name="bar"`. |
| `find_callers` | Callers of a specific definition (path + name + optional kind). Resolves the definition first (echoed in `definition`), then runs the same name-based scan as `find_references`. |
| `list_files` | Enumerate indexed paths, optional `path_contains` + `language` filters. |
| `find_files` | Fuzzy subsequence filename/path search (fzf/fd-style), ranked by `nucleo-matcher` score. |
| `dependents` | Heuristic reverse-lookup via imports. |
| `status` / `repo_info` | Repo overview: file count, language breakdown, cache directory. |
| `symbol_history` | Cross-commit history of a symbol's structural hash via the outline cache + structural-hash machinery. |

### Graph tools (over the unified typed code-graph — ADR-0001/0002/0003)

| Tool | Purpose |
|---|---|
| `call_graph` | Rooted call BFS from a definition, `callees`/`callers` direction, depth-bounded. Name-keyed. |
| `architecture_map` | Whole-repo map: PageRank + SCC clusters, module/file/symbol granularity. `edges` lanes (calls/imports/inherits) carry provenance + confidence. |
| `neighbors` | N-hop neighborhood around a symbol. `direction` (out/in/both), `edges` lanes (all/calls/imports/inherits/both/contains), `depth`, `min_confidence`, `max_nodes`. Two-phase: discover nodes, then induced edges among them. |
| `path` | Confidence-weighted shortest path between two symbols (integer Dijkstra). Containment excluded by default (`include_contains` to add). Returns ordered nodes/edges + total `cost`. |
| `subgraph` | Neighborhood around a symbol cut to the `max_nodes` most central nodes (weighted degree). Roots always kept. Edges carry provenance/confidence. |
| `communities` | Cluster the graph into de-facto modules. `algorithm` (label_propagation default / louvain opt-in), deterministic LLM-free labels (dominant path prefix + most central member). Largest first, capped. |
| `graph_export` | Render the graph as text over one canonical payload: `format` node_link (default) / dot / mermaid / graphml / cypher. `focus`/`edges`/`algorithm`/`min_confidence`/`max_nodes`. Deterministic, offline (no CDN). SVG + interactive HTML deferred to the UI ADRs. |

### Git tools (require `basemind serve` inside a git repo)

| Tool | Purpose |
|---|---|
| `working_tree_status` | `git status` summary with staged / unstaged classification. |
| `recent_changes` | Recent commits with paths + summaries. |
| `commits_touching` | Commits that modified a given path. |
| `find_commits_by_path` | Path-filtered commit log. |
| `diff_file` / `diff_outline` | File and outline diffs across revs. |
| `hot_files` | Churn-ranked files. |
| `blame_file` / `blame_symbol` | Per-line and per-symbol blame. |

#### Contract rules

- All paths are `RelPath` (byte-precise, repo-relative). Do not accept arbitrary `String` paths.
- Responses are `JsonSchema`-derived and stable; new fields are additive with `#[serde(default)]`.
- Lists are capped (`limit`, default 100, max 1000). Index scans use `scan_cap = limit * 8` to bound work on common names.
- Tool descriptions are the routing surface for agents; state semantics (substring vs prefix, scope-aware vs name-only) explicitly.
- Tool bodies live in `src/mcp/helpers*.rs` (sliced by area: `helpers_documents.rs`, `helpers_calls.rs`, `helpers_graph.rs`, `helpers_grep.rs`, `helpers_impls.rs`, `helpers_web.rs`); `tools.rs` and the `tools_<area>.rs` siblings contain `#[tool]` shims only.
