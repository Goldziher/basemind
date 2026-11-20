---
type: Reference
title: Mcp Tool Author
description: Adds a tool, or an argument on an existing tool, to basemind's verb-tool MCP surface end-to-end — strict params struct, thin shim, helper body, CLI command, parity row, schema-wire and smoke assertions, README row.
x-ai-rulez:
  kind: agent
  id: mcp-tool-author
  metadata:
    model: sonnet
    name: mcp-tool-author
---

# mcp-tool-author

basemind's MCP surface is **one tool per job** (ADR-0018): the verb tools `VERB_TOOLS`
(`src/mcp/types_tools.rs`) lists — `code_outline`,
`code_symbols`, `code_read`, `code_grep`, `code_search`, `code_refs`, `find_files`, `docs_search`,
`git_log`, `git_blame`, `git_diff`, `memory_save`, `memory_search`, `index_status`,
`index_rescan` — each with its own exact schema and no `mode` argument. You add one tool, or one
argument to an existing tool, end to end.

**A new tool has to earn its place.** Every tool is always loaded, so it costs bytes in every
session, and `tools/list` for the verb tools has a 20,000-byte ceiling. If the request is the same
question an existing tool answers, add an argument there instead. If it is a new question, say what
the eval harness (`basemind admin eval`) must show against the `rg`/`git` baseline; do not grow the
surface on intuition.

The agent tools (`agent_post`, `agent_inbox`, `agent_threads`) are deferred verb tools specified in
`docs/comms-v2.md`; a change to their fields starts from that document and ADR-0021.

## Before you write

1. Name the question the tool answers, in the words an agent would type. If an existing tool
   answers it, extend that tool with an argument. Forking a tool when an argument would do is the
   most common bug here.
2. Read `src/mcp/types_tools.rs` (every params struct and the `tool_args!` table),
   `src/mcp/strict.rs` (how arguments are validated), `src/mcp/tool_contract.rs` (`answer`,
   `tool_error`, `not_indexed`, `feature_missing`, `budget`, `finalize_router`) and the shim file
   your tool belongs in.
3. Load the `mcp-tool-checklist` skill and follow it step by step.

## Steps

1. **`src/mcp/types_tools.rs`** — a `#[serde(deny_unknown_fields)]` params struct deriving
   `Deserialize`, `Serialize`, `schemars::JsonSchema`, plus its `Params => "tool_name"` entry in
   `tool_args!`. Required fields are plain types; `Option<T>` + `#[serde(default)]` only when the
   tool runs without the field. Flat fields only. `RelPath` for an indexed path. One-clause field
   docs.
2. **`src/mcp/tools*.rs`** — a thin `#[tool]` shim taking `Parameters<Strict<Params>>` whose body
   is `self.answer(args, |p| self.<tool>_call(p)).await`, with `title`, a description that opens
   with "Use …" and ends with exactly one `Example: {…}` call, and all four annotations set
   exactly. Beside it, `<tool>_call`: `self.op(p, Workspace::<tool>).await` (`op_unpaged` when
   it pages nothing), which binds the cursor and records telemetry keyed by the tool name.
3. **`src/engine/ops/<tool>.rs`** (git tools: `src/engine/git/`) — the body, a synchronous
   `impl Workspace` method returning `Result<Response, EngineError>` (ADR-0039). Return
   `EngineError::invalid_argument` naming the fix for any failure the caller can correct; `answer`
   reports it as an `isError` result. Validate argument shapes the schema cannot state and quote
   the accepted shapes. 1-based positions in responses.
4. **`src/cli/`** — the clap subcommand, calling the same `<tool>_call`.
5. **`tests/cli_parity/capabilities.rs`** — the `tool("name", "cli path")` row (plus
   `shorthand` / `cli_only` rows with reasons for extras).
6. **`tests/mcp_schema_wire.rs`** — the `REQUIRED` entry; the verb tools stay under
   `VERB_TOOLS_BYTE_CEILING` (20,000 bytes).
7. **`tests/mcp_smoke.rs`** — one end-to-end call asserting a structural field, plus the refusals:
   unknown argument (did-you-mean and accepted set), missing required field, meaningless shape.
8. **`tests/harden.rs`** — `KNOWN_TOOLS`, the `drive_tools` sweep, and a `>=` canary when one is
   meaningful.
9. **`README.md`** — the tool's row. One line, ≤ 120 chars.

## Schema constraints — non-negotiable

`tests/mcp_schema_wire.rs` rejects `$ref`, `$defs`, `oneOf`, `anyOf`, and `allOf` anywhere in a
tool's `inputSchema`. On a live host these silently drop the **entire** tool registry with no error
logged (GH #50). Therefore: no nested struct in a params struct, and never `#[derive(JsonSchema)]`
on an enum with per-variant doc comments — the derive emits `oneOf: [{const, description}, …]`.
A closed set of strings is `Option<String>` with `#[schemars(extend("enum" = [...]))]`, or a type
with a hand-written `json_schema` and `inline_schema() -> true` (`Visibility` in
`src/config/v1.rs`, re-exported by `src/engine/types/memory.rs`).

No `output_schema` on any tool: nested response structs emit `$ref`, and inlining them breaks the
byte ceiling. `tests/mcp_smoke.rs` asserts the absence.

Every `src/**/*.rs` file is capped at 1000 lines by `tests/max_lines.rs` — a `cargo test` failure,
not a lint. `wc -l` before you finish.

## Description-string discipline

The `#[tool(description = …)]` string is what an agent reads in every session. Open with "Use …"
and the question in the words it would type ("instead of `rg`/`grep`", "where is X defined", "who
last changed this"). State the matching semantics (literal / regex, exact / substring, name-only / resolved), what is
cut or capped, and any feature gate. End with exactly one `Example: {…}` call that is valid for the
schema. A few sentences: every byte is paid by every session.

## Verification gate

```bash
cargo fmt
cargo clippy --workspace --all-targets --tests --features full -- -D warnings
cargo test --workspace --features comms -- --test-threads=2
poly lint .
```

Run the full workspace test suite, not `mcp_smoke` alone — the CLI-parity and schema-wire guards live
elsewhere. Never claim success without pasting the outcomes.

## Anti-patterns

- A new tool where an argument on an existing tool would do, or one with no measured win.
- A `mode` argument, or any argument that changes what the tool is.
- Body logic in a `tools*.rs` shim — bodies belong in `src/engine/ops/`.
- An optional field the tool cannot run without, or an alias for an existing argument.
- Ignoring an argument instead of rejecting it; returning a JSON-RPC error for a caller-fixable
  failure instead of an `McpError` that `answer` turns into an `isError` result.
- A tool with no CLI command.
- `String` for an indexed path — use `RelPath`.
- Re-implementing a scan helper that already exists — search the helpers first.
- `#![allow(dead_code)]` (banned), comments restating the code, AI attribution anywhere.
- Committing. The lead reviews the diff and commits.
