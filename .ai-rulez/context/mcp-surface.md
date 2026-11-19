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
from prose. A `comms` build adds three deferred agent tools (ADR-0021, `docs/comms-v2.md`).

## Verb tools

| Tool | Arguments (required first; `?` = optional) | Job |
|---|---|---|
| `code_outline` | `path`, `limit?`, `cursor?`, `max_tokens?` | a file's symbols (kind, signature, 1-based `line` / `end_line`) and imports |
| `code_symbols` | `name`, `kind?`, `fuzzy?`, `limit?`, `cursor?`, `max_tokens?` | find a definition by name; ranked exact, prefix, substring, then (with `fuzzy`) fuzzy subsequence; a row is `"<path>:<line>: <signature>"` (`"<path>:<line>: <kind> <name>[; <signature>]"` when the signature lacks the name), in rank order; `total` only beside `next_cursor`; files edited since the scan are listed in `stale`, their rows at the current `line` |
| `code_read` | `path`, then `symbol?` (+ `line?` / `kind?` when several share the name) or `line?` + `end_line?` | one symbol's body, or a 1-based inclusive line span; cut after a whole line at 128 KiB (resume at `end_line + 1`); a single line longer than that is cut inside it |
| `code_grep` | `pattern`, `regex?`, `path?`, `language?`, `context?`, `limit?`, `cursor?`, `max_tokens?` | exact text search over indexed code and text documents (markdown, config, data; not PDFs or Office files); literal unless `regex` (`^` / `$` then match at every line); `path` is a literal substring, never a glob; a filter no indexed file passes is an error; `skipped_files` counts unsearchable files; `hits` is `{"<path>": {"<symbol>": ["<line>: <text>", ...]}}` by innermost enclosing symbol (`""` outside every symbol), matches sharing a line one row `"<line>:<col>,<col>: <text>"`, `context` N (0-10) lines around each hit as rows `"<line>- <text>"`, each once; a line over 400 bytes is cut around the match; `total` counts the matches from the cursor on, only beside `next_cursor` |
| `code_search` | `query`, `lane?`, `limit?`, `cursor?`, `max_tokens?` | search code by meaning; returns pointers, not bodies; reports `lanes_ran` and `lanes_unavailable` (lane + reason) |
| `code_refs` | `name`, `relation?`, `path?`, `line?`, `column?`, `match?`, `limit?`, `cursor?`, `max_tokens?` | usages: call sites of a name (matched by name: `resolution` says so; type uses and `use` lines are not call sites), callers of one definition (`path`: returns `definition` and `resolved_total` + per-hit `resolved`, or a `resolution` note: `unsupported for <language>` / `no resolution facts for this file`; add `line` when several definitions share the name), `implementations`, `importers` (with `path`: `by_depth`, `in_cycle` counting importers in a cycle with the file, `deeper: true` when more lie past `depth`), or the `definition` a reference binds to; `match` is `exact` (default: the identifier is the name, or a qualified path ending in it) or `substring`; a call site or implementation in a file edited since the scan has `stale` |
| `find_files` | `query?`, `path_prefix?`, `language?`, `limit?`, `cursor?`, `max_tokens?` | fuzzy file-name search, or a listing without `query`; code and documents; `files` is a list of paths (with `min_lines`, `{"<path>": <lines>}`, largest first) |
| `docs_search` | `query`, `mime_type?`, `kind?`, `path?`, `keywords?`, `entity?`, `limit?`, `cursor?`, `max_tokens?` | semantic search over indexed documents; `path` is a glob over document paths |
| `git_log` | `path?`, `query?`, `author?`, `symbol?`, `line?`, `rev?`, `limit?`, `cursor?`, `max_tokens?` | recent commits, a path's history, a commit search (`query` on the message, `author` on the author), a symbol's history (one window of commits per page, `next_cursor` continues past it), or one commit; a list row is `"<sha12> <summary>"` (`"<sha12> <author>: <summary>"` when `author` is given; `[introduced]` / `[removed]` after the sha in a symbol's history); the recent and directory lists add `files: {"<sha>": {"<path>": "<change>"}}`; author, time and body come from `rev` |
| `git_blame` | `path`, `line?` + `end_line?`, `symbol?`, `rev?`, `limit?`, `cursor?`, `max_tokens?` | per-line authorship of a file, a span, or one symbol (located at `rev`); `hunks` are `"<line>[-<end_line>] <sha12>[ <source_path>]"`, adjacent lines of one commit merged (`limit` counts merged hunks), `commits` maps each sha to `author`, `time`, `summary` |
| `git_diff` | `path?`, `from?`, `to?`, `structural?` | no arguments: working-tree status (200 paths per list; a `path` ending in `/` narrows it to that directory); `path`: that file's diff (`from` defaults to HEAD, `to` to the working tree); `structural`: symbols added and removed since `from` plus the `common` count, no `to`; at most 200 hunks or 128 KiB, then `truncated`; `binary` for a binary file; a `path` in neither revision is an error |
| `memory_save` | `key`, `value`, `tags?`, `visibility?` | write a durable note |
| `memory_search` | `query?` or `key?` or `prefix?`, `tag?`, `visibility?`, `limit?`, `cursor?` | recall notes by meaning, by key, or by key prefix; a listing cut sets `scan_cap`; a `key` no note has answers `{"key":…,"found":false}` |
| `index_status` | none | index health and repository identity (files per language, scan state, `cache_dir` for this workspace, `grammar_dir`, branch, HEAD) |
| `index_rescan` | `paths?` | re-index the given files or directories (a directory's deleted files are pruned), or with none the whole workspace (unchanged files are skipped); a path neither on disk nor indexed, or outside the repository, is an error |

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
  into an `isError: true` result whose text names the fix; an overloaded server answers the same way
  (`server_busy`, retry).
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
