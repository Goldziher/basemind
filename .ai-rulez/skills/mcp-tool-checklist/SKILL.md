---
priority: medium
description: "End-to-end checklist for adding a new MCP tool"
---

# MCP Tool Checklist

Use this when adding a new `#[tool]` to gitmind's MCP server. Skipping a step leaves the tool half-wired.

## Steps

1. **`src/mcp/types.rs`**
   - Add `#[derive(Deserialize, Serialize, JsonSchema)] pub(super) struct <Tool>Params { … }`.
   - Add the matching `<Tool>Response`. Use `Option<T>` + `#[serde(default)]` for optional fields.
   - Path inputs use `RelPath`. Limits default to 100, cap at 1000.

2. **`src/mcp/tools.rs`**
   - Add an `async fn <tool>(&self, Parameters(p): Parameters<<Tool>Params>) -> Result<CallToolResult, McpError>`.
   - Annotate with `#[tool(description = "...")]`. Description states matching semantics (substring vs prefix), scope-awareness, and any caps.
   - Body is `helpers::run_<tool>(&self.state, p).await.map(IntoCallToolResult::into)`.
   - Confirm `tools.rs` stays under 1000 lines (`wc -l src/mcp/tools.rs`).

3. **`src/mcp/helpers.rs`**
   - Implement `pub(super) async fn run_<tool>(state: &State, p: <Tool>Params) -> Result<<Tool>Response, McpError>`.
   - Reuse shared helpers (`scan_calls_by_name`, `resolve_call_line_col`, etc.) where applicable.
   - Apply `scan_cap = limit * 8` when iterating an index range to bound work on common names.

4. **`tests/mcp_smoke.rs`**
   - Extend the synthetic fixture if needed (e.g. files / call sites required by the new tool).
   - Add an end-to-end call; assert response count and at least one structural field (path, line, kind, …).

5. **`tests/harden.rs`**
   - Add the new tool to the per-repo sweep loop so every harden run exercises it.
   - If a canonical canary exists (e.g. `find_references("spawn")` for tokio), add a `>= N` assertion. Use lower bounds, never equality.

6. **`README.md`**
   - Add a row in the MCP tools table. One line, ≤ 120 chars (markdownlint cap).

## Verification

- `cargo test --workspace` — green.
- `cargo clippy --workspace --all-targets --tests -- -D warnings` — clean.
- `prek run -a` — clean.
- `GITMIND_HARDEN_NO_BUILD=1 cargo test --release --test harden -- --ignored --nocapture` — 8/8 green; new canary passes.
