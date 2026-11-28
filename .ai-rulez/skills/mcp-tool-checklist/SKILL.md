---
type: Playbook
title: Mcp Tool Checklist
description: End-to-end checklist for adding a tool, or an argument on an existing tool, to basemind's MCP surface
x-ai-rulez:
  kind: skill
  id: mcp-tool-checklist
  metadata:
    priority: medium
---

# MCP Tool Checklist

Use this when extending basemind's MCP surface. Skipping a step leaves the tool half-wired.

**The surface is one tool per job** (ADR-0018): the verb tools `VERB_TOOLS`
(`src/mcp/types_tools.rs`) lists, each with its own exact schema and no `mode` argument. Decide first whether the capability is an **argument on an existing tool**
(the same question, narrowed: `git_log` took `symbol` and `rev` instead of becoming three tools) or
a **new tool** (a question no existing tool asks). A new tool ships only if the eval harness
(`src/eval/`, `basemind admin eval`) shows it beats the `rg`/`git` baseline on correctness or
tokens, because every tool is always loaded and costs bytes in every session.

The three agent tools (`--features comms`) are verb tools too, but deferred and specified in
`docs/comms-v2.md` (ADR-0021).

Every file stays under the **1000-line cap** enforced by `tests/max_lines.rs` — split an operation
into a submodule of `crates/basemind-engine/src/engine/` rather than raising it.

## Steps

1. **`src/mcp/types_tools.rs`** — add the params struct and its `tool_args!` entry:

   ```rust
   /// Arguments of `code_outline`.
   #[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
   #[serde(deny_unknown_fields)]
   pub struct CodeOutlineParams {
       /// Repo-relative file path.
       pub path: RelPath,
       /// Max symbols (default 500).
       #[serde(default)]
       pub limit: Option<u32>,
   }
   ```

   `tool_args! { …, CodeOutlineParams => "code_outline" }` implements `ToolArgs` (the name quoted
   in argument errors) and adds the tool to `VERB_TOOLS`. Required fields are plain types; a field
   is `Option<T>` + `#[serde(default)]` only when the tool runs without it. Keep the struct flat:
   `tests/mcp_schema_wire.rs` forbids `$ref` / `$defs` / `oneOf` / `anyOf` / `allOf`, schemars
   emits `$ref` for a nested type, and one such construct silently drops the entire registry
   (GH #50). A closed set of strings is `Option<String>` with
   `#[schemars(extend("enum" = [...]))]`, parsed in the body. `RelPath` for an indexed path.
   A list takes `limit`, `cursor: Option<Cursor>` and `max_tokens`. Field docs are one clause:
   they are the schema descriptions an agent reads every session.

2. **`src/mcp/tools.rs` / `tools_git.rs` / `tools_memory.rs` / `tools_admin.rs`** — add the shim:

   ```rust
   #[tool(
       title = "Outline a file",
       description = "Use to see what a file contains before, or instead of, reading it: … \
       Example: {\"path\":\"crates/basemind-engine/src/store.rs\"}",
       annotations(
           read_only_hint = true,
           destructive_hint = false,
           idempotent_hint = true,
           open_world_hint = false
       )
   )]
   pub(crate) async fn code_outline(
       &self,
       Parameters(args): Parameters<Strict<CodeOutlineParams>>,
   ) -> Result<CallToolResult, McpError> {
       self.answer(args, |p| self.code_outline_call(p)).await
   }
   ```

   `Strict<T>` (`src/mcp/strict.rs`) rejects an unknown argument with a did-you-mean and the
   accepted list; `answer` (`src/mcp/tool_contract.rs`) reports that, and any `McpError` the body
   returns, as an `isError` tool result. Beside the router block add the `<tool>_call` method both
   the shim and the CLI run: `self.op(p, Workspace::code_outline).await`, or `op_unpaged` for a
   tool that pages nothing. `src/mcp/adapter.rs` runs the operation on a blocking thread, binds its
   cursor and records the telemetry row under the tool name.
   Set all four annotations to what this tool does; a tool that writes sets
   `read_only_hint = false`. `finalize_router` adds `_meta["anthropic/alwaysLoad"]` to every tool,
   so do not set it per tool.

3. **`crates/basemind-engine/src/engine/ops/<tool>.rs`** (git tools: `crates/basemind-engine/src/engine/git/`) — write the body as a synchronous
   `impl Workspace` method returning `Result<Response, EngineError>`; it names no rmcp type
   (ADR-0039), and response types live in `crates/basemind-engine/src/engine/types/<area>.rs`. Return
   `EngineError::invalid_argument` with text that names the fix for anything the caller can
   correct; use `engine::files::not_indexed_in` (or `indexed_file`) for a path the code map cannot
   serve and `EngineError::feature_missing` for a compiled-out feature.
   Check shapes the schema cannot state ("exactly one of `symbol` or `line`") here and quote the
   accepted shapes in the error. Default the budget with `engine::budget::budget(p.max_tokens)`.
   Positions in a response are 1-based.

4. **`src/cli/`** — add the clap subcommand in the matching group (`code.rs`, `git.rs`,
   `memory.rs`, `admin.rs`) and call the same `<tool>_call`. Every MCP tool has a CLI command.
   The CLI may add shorthands that pre-set arguments (`git status` is `git diff` with none).

5. **`tests/cli_parity/capabilities.rs`** — add `tool("code_outline", "code outline")`. A
   shorthand is a `shorthand(tool, cli, reason)` row; a command with no MCP tool is
   `cli_only(cli, reason)`. The coverage test prints the exact row to add or delete.

6. **`tests/mcp_schema_wire.rs`** — add `("code_outline", &["path"])` to `REQUIRED` (and bump the
   array length). The same file holds the 20,000-byte ceiling for the verb tools in `tools/list`
   (`VERB_TOOLS_BYTE_CEILING`) and the 4,096-character ceiling for the server instructions. If a
   new tool pushes past either, shorten a description; never raise the ceiling.

7. **`tests/mcp_smoke.rs`** — add an end-to-end call against the synthetic fixture asserting a
   structural field, and add the tool to its `*_tools_reject_inexact_arguments` test: an unknown
   argument names the field it most likely meant and the accepted set, a missing required field is
   named, and a meaningless shape names the shapes that have meaning. Derive the asserted wording
   from a real run; do not guess it.

8. **`tests/harden.rs`** — add the tool to `KNOWN_TOOLS` and call it in `drive_tools`. If a
   canonical canary exists (`code_refs("spawn")` on tokio), assert a lower bound (`>=`), never
   equality — upstream repos churn.

9. **`README.md`** — add the tool's row. One line, ≤ 120 chars.

Also update `src/mcp/server_handler.rs` instructions when an agent needs to know the tool exists,
`src/mcp/tasks.rs` `SLOW_CALLS` when it routinely runs for seconds, and `crates/basemind-engine/src/engine/savings.rs` when
it has a measured shell baseline.

## Constraints not to "fix" locally

- **No `output_schema`.** The response structs nest, nested structs emit `$ref`, and inlining them
  puts `tools/list` over its ceiling. `tests/mcp_smoke.rs` asserts the absence on every verb tool.
- **Exact annotations per tool.** A host that auto-approves read-only tools must be able to allow
  `git_log` without allowing `index_rescan`.
- **No aliases.** One spelling per argument; a retired name is rejected like any unknown field.

## The description

It opens with "Use …" and the question the tool answers in the words an agent would type ("instead
of `rg`/`grep`", "where is X defined", "who last changed this"), states the contract (literal vs
regex, exact vs substring, name-only vs resolved, what is cut or capped), and ends with exactly one `Example: {…}`
call that is valid for the schema. Keep it to a few sentences: the byte ceiling is shared.

## Verification

- `cargo test --workspace` — green. Check cargo's own exit code, not a pipeline's; `| tail` reports
  `tail`'s status and will happily show you a passing-looking log for a failed run.
- `cargo clippy --workspace --all-targets --tests -- -D warnings` — clean.
- `poly lint .` — clean. If its `uncomment` lint flags a genuine why-comment, add `~keep` inside the
  comment (every line of a multi-line block) rather than deleting the explanation.
- `cargo test --test max_lines` — no file over the cap.
- `cargo test --test mcp_schema_wire` — no forbidden schema construct; `REQUIRED` lists every verb
  tool; byte and instruction ceilings hold.
- `cargo test --test cli_parity` — every tool has its CLI command and every row resolves.
- `BASEMIND_HARDEN_NO_BUILD=1 cargo test --release --test harden -- --ignored --nocapture` — 8/8
  green; new canary passes.
