# Paging and budgets

Every list takes `limit` and `cursor` and returns `next_cursor` while more remains. Pass
`next_cursor` back as `cursor` (CLI: `--cursor`) with the same arguments; only `limit` and
`max_tokens` may change between pages.

- **Defaults.** `code_symbols`, `code_grep` and `code_refs` list 100 rows, `find_files` 200,
  `code_outline` 500 symbols, `code_search` and `docs_search` 10 hits, `git_log` 20 commits (at
  most 100), `git_blame` 100 hunks. `limit: 0` is refused.
- **Token budget.** A list is trimmed to `max_tokens` (default 6000) and then sets `budgeted`:
  raise `max_tokens` or follow `next_cursor`.
- **Three words for a cut.** A page that ended at `limit` returns `next_cursor` and nothing else.
  `budgeted` means the token budget trimmed it. `truncated` with `truncated_reason` marks a cut you
  did not ask for, with `omitted` counting what was left out where it is known; the codes and
  what to do about each are listed under "Truncation reasons" below.
- A `limit` above its maximum is capped; each argument's description states both. `find_files` sets `limit_clamped`.
- **Cursors are bound to their query.** A cursor continues only the call that returned it. Passed
  to another query, or used after a rescan or a HEAD move, the call is an error: repeat it without
  `cursor`. No tool answers a stale cursor with an empty page.
- **Notices.** An empty or partial answer carrying a `notice` (`empty_index`, `warming_up`,
  `building_index`, `rescanning`) means "retry shortly", not "no matches".
- **Text answers.** The text a model reads is written per tool, in the form that measured fewest
  tokens (ADR-0053). `code_grep` writes its other fields as `key: value` lines, then each file as
  `# <path>` and each enclosing symbol as `## <symbol>` over its rows; rows before any symbol
  header, or under a bare `##`, are outside every symbol. `code_outline`, `git_diff` and a
  `find_files` list are [TOON](https://toonformat.dev): `key: value` lines, and a list of objects
  as one table, `symbols[N]{name,kind,...}:` and a comma-separated row per object, a value quoted
  only where it would be misread. Every other tool answers in compact JSON. A header naming a path
  or symbol that starts with `"`, has leading or trailing whitespace or holds a control character
  is a JSON string. MCP `structuredContent` is JSON for every tool.
- **CLI output.** Human output prints bodies and diffs in full and cuts only table cells, with a
  note on stderr. `--json` prints the response as JSON, as MCP `structuredContent` carries it.

## Truncation reasons

`truncated_reason` says why an answer is incomplete.

| Reason | Tool | Next step |
| --- | --- | --- |
| `scan_cap` | `code_symbols`, `code_refs`, `memory_search`, `impact` | `total` is a lower bound: follow `next_cursor`, else narrow with `path` or `language`. |
| `byte_budget` | `code_grep` | Follow `next_cursor`, or narrow with `path` or `language`. |
| `time_budget` | `code_grep` | Sweep stopped at 10 s (see `notice`): follow `next_cursor`, or narrow. |
| `depth` | `code_search`, `docs_search`, `impact` | Past 100 hits, or an import walk at its depth: refine the query. |
| `liveness_depth` | `docs_search` | Narrow with `path`, `keywords` or `format`. |
| `stale_chunks` | `code_search` | Hits in files edited since indexing were dropped (`omitted`): `index_rescan` them. |
| `imports` | `code_outline` | Import list cut at 200: `code_grep` for the import. |
| `size_cap` | `code_read` | Read a span with `line` and `end_line`. |
| `too_large` | `git_blame` | Blame a span or one `symbol`. |
| `hunk_cap`, `byte_cap` | `git_diff` | Stopped at 200 hunks or 128 KiB, no cursor: narrow `from` and `to`. |
| `path_cap` | `git_diff` | Status list cut (`omitted`): pass a directory `path` ending in `/`. |
| `span_limit` | `git_log` | Follow `next_cursor`, an empty page included, until it is absent. |
| `unindexed_scan` | `git_log` | No history index, newest commits only: wait for it (`index_status`), repeat. |
| `shallow_clone` | `git_log`, `git_blame` | An absent commit is inconclusive: `git fetch --unshallow`.
