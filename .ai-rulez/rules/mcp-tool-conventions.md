---
priority: high
---

# MCP Tool Conventions

Every MCP tool follows the same wiring path. Adding a tool means touching all of these files in order; skipping a step leaves the tool half-wired.

1. **`src/mcp/types.rs`** (or matching `types_<area>.rs` — `types_documents.rs` / `types_graph.rs` / `types_impls.rs`) — define `<Tool>Params` and `<Tool>Response` structs. Derive `Deserialize`, `Serialize`, `JsonSchema`. Use `Option<T>` + `#[serde(default)]` for optional params with sensible defaults. Reuse `RelPath` for path fields; do not accept arbitrary `String` paths.
2. **`src/mcp/tools.rs`** (or matching `tools_<area>.rs` — `tools_admin.rs` / `tools_git.rs` / `tools_memory.rs` / `tools_web.rs`) — add an `async fn <tool>(...)` annotated with `#[tool(description = "...")]`. The body MUST be a thin wrapper delegating to a helper. Keep each file under the 1000-line cap.
3. **`src/mcp/helpers*.rs`** (pick the matching slice — `helpers_documents.rs` / `helpers_calls.rs` / `helpers_graph.rs` / `helpers_grep.rs` / `helpers_impls.rs` / `helpers_web.rs`, or the catch-all `helpers.rs`) — implement the body as `run_<tool>(state, params) -> Result<<Tool>Response, McpError>`. Helpers may share scan / decode / cap functions with sibling tools.
4. **`tests/mcp_smoke.rs`** — add an assertion: the synthetic fixture exercises the tool end-to-end and checks a stable response shape.
5. **`tests/harden.rs`** — add a sweep call in the tool-sweep loop and (when meaningful) a per-repo canary, e.g. `tokio: find_references("spawn") >= 200`.
6. **`README.md`** — add a row in the MCP tools table with the one-line purpose.

Tool descriptions should state the contract honestly: what matching semantics (substring vs prefix), what's resolved (scope-aware vs name-only), and what's capped (`scan_cap = limit * 8` for the index scanners). Agents make routing decisions from the description string.
