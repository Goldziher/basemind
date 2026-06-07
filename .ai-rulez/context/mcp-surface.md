---
priority: high
---

# MCP Surface

`gitmind serve` exposes a stdio MCP server (`rmcp`). The live contract is `tests/mcp_smoke.rs` — read it before changing any tool's response shape.

## Code-map tools

| Tool | Purpose |
|---|---|
| `outline` | Full per-file structure: symbols + line/col + signatures + imports. `l2: true` includes calls + docs. |
| `search_symbols` | Substring lookup across every indexed file, with optional kind filter. In-RAM `memmem`. |
| `find_references` | Call sites of any callee whose identifier matches `name`. Backed by Fjall `calls_by_callee`. No scope resolution; `Foo::bar()` and `bar()` both match `name="bar"`. |
| `find_callers` | Callers of a specific definition (path + name + optional kind). Resolves the definition first (echoed in `definition`), then runs the same name-based scan as `find_references`. |
| `list_files` | Enumerate indexed paths, optional `path_contains` + `language` filters. |
| `dependents` | Heuristic reverse-lookup via imports. |
| `status` / `repo_info` | Repo overview: file count, language breakdown, cache directory. |
| `symbol_history` | Cross-commit history of a symbol's structural hash via the outline cache + structural-hash machinery. |

## Git tools (require `gitmind serve` inside a git repo)

| Tool | Purpose |
|---|---|
| `working_tree_status` | `git status` summary with staged / unstaged classification. |
| `recent_changes` | Recent commits with paths + summaries. |
| `commits_touching` | Commits that modified a given path. |
| `find_commits_by_path` | Path-filtered commit log. |
| `diff_file` / `diff_outline` | File and outline diffs across revs. |
| `hot_files` | Churn-ranked files. |
| `blame_file` / `blame_symbol` | Per-line and per-symbol blame. |

## Contract rules

- All paths are `RelPath` (byte-precise, repo-relative). Do not accept arbitrary `String` paths.
- Responses are `JsonSchema`-derived and stable; new fields are additive with `#[serde(default)]`.
- Lists are capped (`limit`, default 100, max 1000). Index scans use `scan_cap = limit * 8` to bound work on common names.
- Tool descriptions are the routing surface for agents; state semantics (substring vs prefix, scope-aware vs name-only) explicitly.
- Tool bodies live in `src/mcp/helpers.rs`; `tools.rs` contains `#[tool]` shims only.
