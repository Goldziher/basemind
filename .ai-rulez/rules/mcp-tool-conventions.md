---
type: Decision
title: Mcp Tool Conventions
x-ai-rulez:
  kind: rule
  id: mcp-tool-conventions
  metadata:
    priority: high
---

# MCP Tool Conventions

The surface is one tool per job (ADR-0018): fifteen verb tools, each with an exact schema and no `mode` argument. A new capability is first an **argument on an existing tool** when the question is the same (`git_log` grew `symbol` and `rev` rather than two tools); it is a **new tool** only when no existing tool asks that question, and only if the eval harness (`basemind admin eval`) shows it beats the `rg`/`git` baseline on correctness or tokens. Every tool is always loaded, so each one costs bytes in every session: `tools/list` for the verb tools has a 20,000-byte ceiling. Skipping a step below leaves the tool half-wired; see the `mcp-tool-checklist` skill for the full walkthrough.

1. **`src/mcp/types_tools.rs`** — add a `#[serde(deny_unknown_fields)]` params struct deriving `Deserialize`, `Serialize` and `JsonSchema`, and its `Params => "tool_name"` entry in `tool_args!` (which implements `ToolArgs` and extends `VERB_TOOLS`). Required fields are plain types; a field is `Option<T>` + `#[serde(default)]` only when the tool runs without it. Flat fields only: `tests/mcp_schema_wire.rs` forbids `$ref` / `$defs` / `oneOf` / `anyOf` / `allOf`, and one such construct silently drops the whole registry (GH #50). `RelPath` for a path that must exist in the index, never an arbitrary `String`. Field docs are one clause: they are the schema descriptions and count against the byte ceiling.
2. **`src/mcp/tools*.rs`** — add a thin `#[tool]` shim taking `Parameters<Strict<Params>>` whose whole body is `self.answer(args, |p| self.<tool>_call(p)).await`, with a `title`, a `description` that opens with "Use …" and ends with exactly one `Example: {…}` call, and all four annotations set to what this tool does. Beside it add `<tool>_call`, which wraps the body in `instrument_tool!` keyed by the tool name.
3. **`src/mcp/helpers_*.rs`** — write the body. Return `McpError` for a failure the caller can fix, with text naming the fix; `answer` reports it as an `isError` result. Check argument shapes the schema cannot state ("exactly one of") here and name the accepted shapes in the error. Default the token budget with `tool_contract::budget`.
4. **`src/cli/`** — add the clap command in the matching group, calling the same `<tool>_call`. Every MCP tool has a CLI command; the CLI may add shorthands and CLI-only commands.
5. **`tests/cli_parity/capabilities.rs`** — add the `tool("name", "cli path")` row, and a `shorthand(...)` or `cli_only(...)` row with its reason for anything extra.
6. **`tests/mcp_schema_wire.rs`** — add the tool and its required fields to `REQUIRED`, and keep the serialized verb tools under `VERB_TOOLS_BYTE_CEILING`.
7. **`tests/mcp_smoke.rs`** — add an end-to-end call asserting a structural field, and the refusals: an unknown argument names the field it most likely meant and the accepted set, and a missing required field is named.
8. **`tests/harden.rs`** — add the tool to `KNOWN_TOOLS` and the `drive_tools` sweep, and a canary lower bound (`>=`, never equality) when one is meaningful.
9. **`README.md`** — add the tool's row.

Descriptions state the contract honestly: matching semantics (literal vs regex, exact vs fragment), what is resolved (scope-aware vs name-only), and what is capped. Do not declare an `output_schema`: nested response structs emit `$ref`, and inlining them breaks the byte ceiling; `tests/mcp_smoke.rs` asserts the absence. Annotations are exact per tool: a tool that writes sets `read_only_hint = false`, so a host that auto-approves read-only tools cannot trigger it.

`agents` (`--features comms`) remains mode-dispatched until its own redesign: its vocabulary stays in `src/mcp/mode.rs` (`define_mode!`, `reject_unsupported`), and a new mode there still needs its `pair(tool, mode, cli)` row in `tests/cli_parity/capabilities.rs`.
