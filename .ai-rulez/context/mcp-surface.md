---
type: Concept
title: Mcp Surface
x-ai-rulez:
  kind: context
  id: mcp-surface
  metadata:
    priority: high
---

# MCP Surface

`basemind serve` exposes a stdio MCP server (`rmcp`). The live contract is `tests/mcp_smoke.rs` — read it before changing any tool's response shape.

The surface is **one tool per job** (ADR-0018): the verb tools, each with its own exact schema and
no `mode` argument. `src/mcp/types_tools.rs` is the authoritative list of tools (`VERB_TOOLS`) and
of every argument; read it before writing about a tool or counting them, never infer an argument
from prose. A `comms` build adds three deferred agent tools (ADR-0021, `docs/comms-v2.md`), and a
`basemind serve --lead` session the two lead tools `agent_launch` and `agent_stop`
(`src/mcp/lead_tools.rs`, ADR-0040).

## Verb tools

Each tool's answer shape (rows, fields, cut sizes, notes) is in `docs/mcp-tools.md`; read it, or the
tool's body, before changing one.

| Tool | Arguments (required first; `?` = optional) | Job |
|---|---|---|
| `code_outline` | `path`, `limit?`, `cursor?`, `max_tokens?` | a file's symbols and imports |
| `code_symbols` | `name`, `kind?`, `path?`, `language?`, `fuzzy?`, `limit?`, `cursor?`, `max_tokens?` | find a definition by name |
| `code_read` | `path`, then `symbol?` (+ `line?` / `kind?` when several share the name) or `line?` + `end_line?` | one symbol's body, or a line span |
| `code_grep` | `pattern`, `regex?`, `path?`, `language?`, `context?`, `limit?`, `cursor?`, `max_tokens?` | exact text search over indexed code and text documents |
| `code_search` | `query`, `lane?`, `path?`, `language?`, `limit?`, `cursor?`, `max_tokens?` | search code by meaning; pointers, not bodies |
| `code_refs` | `name`, `relation?`, `path?`, `line?`, `column?`, `match?`, `limit?`, `cursor?`, `max_tokens?` | call sites, callers, implementations, importers, or the definition a reference binds to |
| `find_files` | `query?`, `path?`, `language?`, `limit?`, `cursor?`, `max_tokens?` | fuzzy file-name search, or a listing |
| `docs_search` | `query`, `mime_type?`, `format?`, `path?`, `keywords?`, `entity?`, `limit?`, `cursor?`, `max_tokens?` | semantic search over indexed documents |
| `git_log` | `path?`, `query?`, `author?`, `symbol?`, `line?`, `rev?`, `limit?`, `cursor?`, `max_tokens?` | recent commits, a path's or symbol's history, a commit search, or one commit |
| `git_blame` | `path`, `line?` + `end_line?`, `symbol?`, `rev?`, `limit?`, `cursor?`, `max_tokens?` | per-line authorship of a file, a span, or one symbol |
| `git_diff` | `path?`, `from?`, `to?`, `structural?` | working-tree status, a file's diff, or the symbols added and removed |
| `memory_save` | `key`, `value`, `tags?`, `visibility?` | write a durable note |
| `memory_search` | `query?` or `key?` or `prefix?`, `tags?`, `visibility?`, `limit?`, `cursor?` | recall notes by meaning, key, or key prefix |
| `index_status` | none | index health and repository identity |
| `index_rescan` | `paths?` | re-index files or directories, or the whole workspace |

`code_search` needs the `code-search` Cargo feature, `docs_search` needs `documents`, and
`memory_save` / `memory_search` need `memory`. All four are advertised in every build; a build
without the feature answers with an `isError` result naming it. The git tools require
`basemind serve` inside a git repository.

Off the MCP surface and reachable only from the CLI: `git churn`, `git by-path <regex>`,
`code chunk`, `memory delete`, `cache stats|gc|clear`, `admin telemetry`.

## Contract of every verb tool

- **Exact arguments.** Each params struct is `deny_unknown_fields` and deserialized through
  `Strict<T>` (`src/mcp/strict.rs`): an argument the tool does not take is refused with a
  did-you-mean and the accepted list, never silently dropped. A stringified integer or boolean is
  coerced; any other type error names the field, and unknown keys, type errors and missing fields
  are reported in one message. A field is optional only when the tool runs without it. "Exactly
  one of" constraints cannot be stated in the flat schema subset and are checked in the body; each
  is also a clause on the field's doc.
- **Paths and positions.** `path` is a `RelPath` (repo-relative, forward slashes), never an arbitrary
  `String`. `RelPath` does not normalise, so a body that reads the working tree goes through
  `engine::files::indexed_file` (`IndexedFiles`), which refuses an absolute path, a `..` component
  and a symlink leaving the repository, and consults the index first. `line`,
  `end_line` and `column` are 1-based; a position the index does not hold is omitted, never 0.
- **Lists.** Every list takes `limit` and `cursor` and returns `next_cursor`. A list is trimmed to
  a default budget of 6000 tokens (`DEFAULT_MAX_TOKENS`) and then sets `budgeted`. `limit: 0` is
  refused in `cursor_bind::bound`, once for every tool that takes `limit`.
- **Cuts.** A `limit` cut is paging: `next_cursor` alone. A cut the caller did not ask for sets
  `truncated` + `truncated_reason` (+ `omitted`, items); the token budget sets `budgeted`. Add no
  other name. A filter no file passes is an error (`engine::filters`).
- **Files changed since the scan.** Load through `engine::freshness::load` (re-extract and set `stale`, else
  `freshness::edited`; a deleted file is `freshness::unloadable`).
- **Cursors are bound to their query.** Every paginated `*_call` runs through `cursor_bind::bound`,
  which prefixes `next_cursor` with a hash of the tool name and list-selecting arguments. A cursor
  used on another query, or after its index or HEAD moved, is an `isError` result saying to call
  again without `cursor`; never an empty page.
- **Pinned symbols.** `code_refs` with `path`, `code_read`, `git_blame` and `git_log` with `symbol`
  resolve through `engine::pick::pick_symbol`: a missing file or symbol is an error, and a name
  several definitions share is an error listing each until `line` picks one.
- **Failures are results.** `BasemindServer::answer` (`src/mcp/tool_contract.rs`) turns an `McpError`
  into an `isError: true` result whose text names the fix and whose `structuredContent` carries its
  `error` code and `retryable` flag (`src/engine/error_kind.rs`); an overloaded server answers the same
  way (`busy`, with `retry_after_ms`).
- **Notices.** An empty index is never a plain empty result: a `notice` (`empty_index`,
  `warming_up`, `building_index`, `rescanning`) says why.
- **Inline results.** A Task only for a call still running past `tasks::SLOW_CALLS`
  (`index_rescan`, `code_search`, `docs_search`).
- **Always loaded.** `finalize_router` stamps `_meta["anthropic/alwaysLoad"]: true` on the verb
  tools, so a host that defers MCP tools still loads their schemas at session start. The agent
  tools are not marked and stay deferred (ADR-0021).
- **Descriptions.** Open with "Use …", state the contract, end with one `Example: {…}` call.
- **Annotations are exact per tool.** Everything is read-only except `memory_save` and
  `index_rescan`; nothing is destructive.
- **No `output_schema`.** Nested response structs emit `$ref` into `$defs` (the construct that
  dropped the whole registry in GH #50) and inlining them breaks the byte ceiling;
  `tests/mcp_smoke.rs` asserts the absence.
- **Text answers (ADR-0053).** The tool builds its answer as JSON in `structured_content` (what the in-process CLI reads); on the
  MCP wire a successful answer travels once, in the text block, and `structuredContent` is dropped
  (`tool_contract::single_payload`, applied in `BasemindServer::route`; an `isError` result keeps its
  `{error, retryable}`). The text block is
  `crate::wire::encode(tool, …)`, set in `src/mcp/adapter.rs`: `code_grep` rows as a plain block,
  `code_outline` / `git_diff` / a `find_files` list as TOON, the rest compact JSON.
  `crate::wire::decode` reads any of them back (the eval and the tests read answers with it). A
  list's token budget charges the encoding its tool writes. A resource's text is its tool's
  (`basemind://outline/{path}` is TOON, `text/toon`).
- **Byte ceiling.** The verb tools serialize to at most 20,000 bytes in `tools/list`, the server
  instructions to 4,096 characters (`tests/mcp_schema_wire.rs`); never raise either.
- **Telemetry keys are the tool names;** `src/mcp/savings.rs` maps older keys onto them.

## Layout

Params structs and `VERB_TOOLS`: `src/mcp/types_tools.rs`. Shims and `<tool>_call`:
`src/mcp/tools*.rs`. Bodies: `src/engine/ops/<tool>.rs` and `src/engine/git/`. The CLI (`src/cli/`) calls the same
`<tool>_call` (`tests/cli_parity/`).

## Agent tools (`--features comms`)

`agent_post`, `agent_inbox`, `agent_threads` (ADR-0021; normative: `docs/comms-v2.md`) replace the
`agents` tool and `basemind agents`. Deferred (no always-load marker; found by tool search), no
`mode`, each with an optional `as` (`[a-z0-9_-]{1,32}`, sub-identity `<principal>.<name>`); a field
that does not apply is refused.

| Tool | Arguments (`?` = optional) | Job |
|---|---|---|
| `agent_post` | `body`, one of `thread` / `reply_to` / `subject`; `to?`, `paths?` (with `subject`) | send; `subject` starts a thread, `to` makes it invitation-only |
| `agent_inbox` | `thread?`, `id?`, `unread?`, `peek?`, `wait_secs?` (0..40), `limit?`, `cursor?` | read; bodies inline (4 KiB; `id` reads one whole), reading marks read, `peek` does not |
| `agent_threads` | `query?` (3+), `path?`, `thread?`, one of `join` / `leave` / `invite` / `archive`, `archived?`, `limit?`, `cursor?` | list, describe or change threads |

Principal: `BASEMIND_AGENT_ID`, else `comms.agent_id`, else `<host>-<repo>-<6 hex>`. `inbox
--announce` (front matter, never marks read) is CLI and hook only. CLI: `basemind agent
post|inbox|threads|status|cleanup`.

### Machine registry (CLI only)

`basemind workspace workspaces` / `worktrees` / `branches` list the daemon's machine-wide
registry; there is no `workspace` MCP tool.
