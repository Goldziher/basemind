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

The surface is **one tool per job** (ADR-0018): fifteen verb tools, each with its own exact
schema and no `mode` argument. `src/mcp/types_tools.rs` is the authoritative list of tools
(`VERB_TOOLS`) and of every argument; read it before writing about a tool, never infer an argument
from prose. A `comms` build adds one tool that is still mode-dispatched, `agents`,
until its own redesign.

## Verb tools

| Tool | Arguments (required first; `?` = optional) | Job |
|---|---|---|
| `code_outline` | `path`, `limit?`, `cursor?`, `max_tokens?` | a file's symbols (kind, signature, 1-based `line` / `end_line`) and imports; `stale` when the file changed since it was indexed and was outlined as it is |
| `code_symbols` | `name`, `kind?`, `fuzzy?`, `limit?`, `cursor?`, `max_tokens?` | find a definition by name; ranked exact, prefix, substring, then (with `fuzzy`) fuzzy subsequence; each row carries `match` |
| `code_read` | `path`, then `symbol?` (+ `line?` / `kind?` when several share the name) or `line?` + `end_line?` | one symbol's body, or a 1-based inclusive line span; cut after a whole line at 128 KiB (resume at `end_line + 1`); `stale` when the file changed since it was indexed |
| `code_grep` | `pattern`, `regex?`, `path?`, `language?`, `context?`, `limit?`, `cursor?`, `max_tokens?` | text search over indexed code; literal unless `regex` (`^` / `$` then match at every line); `path` is a literal substring, never a glob; a filter no indexed file passes is an error; `skipped_files` counts unsearchable files; a hit is its line `text` (a line over 400 bytes is cut around the match) plus the enclosing `symbol` |
| `code_search` | `query`, `lane?`, `limit?`, `cursor?`, `max_tokens?` | search code by meaning; returns pointers, not bodies; reports `lanes_ran` and `lanes_unavailable` (lane + reason) |
| `code_refs` | `name`, `relation?`, `path?`, `line?`, `column?`, `match?`, `limit?`, `cursor?`, `max_tokens?` | usages: call sites of a name, callers of one definition (`path`: returns `definition` and `resolved_total` + per-hit `resolved`, or a `resolution` note: `unsupported for <language>` / `no resolution facts for this file`; add `line` when several definitions share the name), `implementations`, `importers`, or the `definition` a reference binds to; `match` is `exact` (default: the identifier is the name, or a qualified path ending in it) or `substring` |
| `find_files` | `query?`, `path_prefix?`, `language?`, `limit?`, `cursor?`, `max_tokens?` | fuzzy file-name search, or a listing without `query`; code and documents |
| `docs_search` | `query`, `mime_type?`, `keywords?`, `entity?`, `scope?`, `limit?`, `cursor?`, `max_tokens?` | semantic search over indexed documents; `truncated` (`depth`, `rerank_top_k`, `filter_depth`) when the ranking was cut |
| `git_log` | `path?`, `query?`, `author?`, `symbol?`, `line?`, `rev?`, `limit?`, `cursor?`, `max_tokens?` | recent commits, a path's history, a commit search (`query` on the message, `author` on the author), a symbol's history (one window of commits per page, `next_cursor` continues past it), or one commit |
| `git_blame` | `path`, `line?` + `end_line?`, `symbol?`, `rev?`, `limit?`, `cursor?`, `max_tokens?` | per-line authorship of a file, a span, or one symbol (located in the file as it is at `rev`) |
| `git_diff` | `path?`, `from?`, `to?`, `structural?` | no arguments: working-tree status (200 paths per list; a `path` ending in `/` narrows it to that directory); `path`: that file's diff (`from` defaults to HEAD, `to` to the working tree); `structural`: symbols added and removed since `from` plus the `common` count, no `to`; at most 200 hunks or 128 KiB, then `truncated`; `binary` for a binary file |
| `memory_save` | `key`, `value`, `tags?`, `visibility?` | write a durable note |
| `memory_search` | `query?` or `key?` or `prefix?`, `tag?`, `visibility?`, `limit?`, `cursor?` | recall notes by meaning, by key, or by key prefix |
| `index_status` | none | index health and repository identity (files per language, scan state, `cache_dir` for this workspace, `grammar_dir`, branch, HEAD) |
| `index_rescan` | `paths?` | re-index the given paths, or with none the whole workspace (unchanged files are skipped) |

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
- **Paths and positions.** `path` is a `RelPath` (repo-relative, forward slashes); do not accept an
  arbitrary `String` path. `RelPath` does not normalise, so a body that reads the working tree goes
  through `tool_contract::working_file` / `indexed_file`, which refuse an absolute path, a `..`
  component and a symlink leaving the repository, and consults the index before reading. `line`,
  `end_line` and `column` are 1-based; a position the index does not hold is omitted, never 0.
- **Lists.** Every list takes `limit` and `cursor` and returns `next_cursor`. A list is trimmed to
  a default budget of 6000 tokens (`DEFAULT_MAX_TOKENS`) and then sets `budgeted`. Index scans
  stay bounded by a scan cap and report `total_is_partial`. `limit: 0` is refused in
  `cursor_bind::bound`, once for every tool that takes `limit`.
- **One vocabulary for a cut.** A hard bound sets `truncated: true` and `truncated_reason`, with
  `omitted` counting what was left out when the count is known; the token budget sets `budgeted`.
  Do not add another field name for "something was left out". Every cut must leave the caller a
  way on: a `next_cursor`, or an argument that narrows the call.
- **Files that changed since the scan.** Indexed byte offsets apply only to the indexed bytes.
  Every body that lays them over the working tree loads the file through `freshness::load` (one
  bounded read, one verdict): a tool that can re-extract from live bytes does and sets `stale`,
  a tool that cannot returns `freshness::edited`, and a deleted file gets `freshness::unloadable`.
  No indexed file is read whole past `scan.max_file_bytes`.
- **Filter refusals.** A `path` / `path_prefix` / `language` filter no listed file passes is an
  error (`helpers_filters`), made only when the listing came back empty and the server has no
  lifecycle notice to attach, from the same source the tool lists from.
- **Cursors are bound to their query.** Every paginated `*_call` runs its body through
  `cursor_bind::bound`, which prefixes `next_cursor` with a hash of the tool name and the
  list-selecting arguments. A cursor passed to another query, or one that outlived its index or
  HEAD, is an `isError` result telling the agent to call again without `cursor`; no tool answers a
  stale cursor with an empty page.
- **Pinned symbols.** `code_refs` with `path`, `code_read`, `git_blame` and `git_log` with `symbol`
  resolve the name through `helpers_pick::pick_symbol`: a missing file or symbol is an error, and a
  name several definitions share is an error listing each with its line span until `line` picks one.
- **Failures are results.** `BasemindServer::answer` (`src/mcp/tool_contract.rs`) turns an `McpError`
  returned by a tool body into a tool result with `isError: true` whose text names the fix. An
  overloaded server answers the same way (`server_busy`, retry). JSON-RPC errors are left to the
  protocol layer.
- **Notices.** An empty index is never a plain empty result: the response carries a `notice`
  (`empty_index`, `warming_up`, `building_index`, `rescanning`).
- **Inline results.** A server-side Task is used only for a call still running after the threshold
  (`tasks::SLOW_CALLS`: `index_rescan`, `code_search`, `docs_search`).
- **Always loaded.** `finalize_router` stamps `_meta["anthropic/alwaysLoad"]: true` on the verb
  tools, so a host that defers MCP tools still loads their schemas at session start. `agents` is
  not marked and stays deferred (ADR-0020).
- **Descriptions.** Each opens with "Use …", states the contract, and ends with one
  `Example: {…}` call. Field docs are one clause each.
- **Annotations are exact per tool.** Everything is read-only except `memory_save` and
  `index_rescan`; nothing is destructive.
- **No `output_schema`.** The response structs nest, schemars emits nested structs as `$ref` into
  `$defs` (the construct that dropped the whole registry in GH #50), and inlining them would break
  the byte ceiling. `tests/mcp_smoke.rs` asserts the absence.
- **Byte ceiling.** The verb tools serialize to at most 20,000 bytes in `tools/list`, and the server
  instructions to at most 4,096 characters (`tests/mcp_schema_wire.rs`). Shorten a description;
  never raise the ceiling.
- **Telemetry keys are the tool names.** `src/mcp/savings.rs` maps the keys older binaries wrote
  (`code:outline`) onto them.

## Layout

- `src/mcp/types_tools.rs` — one params struct per tool, plus the `tool_args!` table that binds
  each struct to its tool name and builds `VERB_TOOLS`.
- `src/mcp/tools.rs` (code tools and `find_files`), `tools_git.rs`, `tools_memory.rs`
  (`docs_search`, `memory_save`, `memory_search`), `tools_admin.rs` (`index_status`,
  `index_rescan`) — one thin `#[tool]` shim per tool, and beside it the `<tool>_call` method that
  both the shim and the CLI run.
- `src/mcp/helpers_*.rs` — the bodies.
- `src/cli/` — every MCP tool has a CLI command that calls the same `<tool>_call`
  (`tests/cli_parity/` enforces it; the mapping is injective, and the CLI may keep shorthands and
  CLI-only administrative commands, each a row in `tests/cli_parity/capabilities.rs`).

## Mode-dispatched tool (`--features comms`)

`agents` dispatches on a required `mode`. Its vocabulary lives in `src/mcp/mode.rs`
(`define_mode!`), with per-mode fields as optional siblings validated by `reject_unsupported`.

### `agents`

`register` / `list` (identity card, roster), `thread_start` / `thread_list` (open / discover
threads addressed by subject, path-glob, and/or members), `join` / `leave` / `members` (explicit
membership — no auto-join), `add_member` / `remove_member` / `archive` (creator/admin-only
thread management), `post` (send a message), `history` / `inbox` (front-matter only — subject /
from / id), `message` (read one body by id — the only path to a body), `ack` (clear read
messages), `wait` (block until a peer posts or the timeout elapses), `cleanup` (preview or apply
stale-record retention), `status` (active and stale agent counts).

### Machine registry (CLI only)

`basemind workspace workspaces` / `worktrees` / `branches` list the daemon's machine-wide
registry. There is no `workspace` MCP tool and there are no worktree claims.
