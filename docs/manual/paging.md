# Paging and budgets

Every list takes `limit` and `cursor` and returns `next_cursor` while more remains. Pass
`next_cursor` back as `cursor` (CLI: `--cursor`) with the same arguments; only `limit` and
`max_tokens` may change between pages.

- **Defaults.** `code_symbols`, `code_grep` and `code_refs` list 100 rows, `find_files` 200,
  `code_outline` 500 symbols, `code_search` and `docs_search` 10 hits, `git_log` 20 commits (at
  most 100), `git_blame` 100 hunks. `limit: 0` is refused.
- **Token budget.** A list is trimmed to `max_tokens` (default 6000), then sets `budgeted`: raise
  `max_tokens` or follow `next_cursor`. A first result over `max_tokens` is still returned, and
  `budgeted` stays unset when nothing was cut.
- **Three words for a cut.** A page ended at `limit` returns only `next_cursor`; `budgeted` means
  the token budget trimmed it; `truncated` with `truncated_reason` marks a cut you did not ask for
  (`omitted` counts what was left out; reasons below).
- **Limit cap.** `git_log` silently caps `limit` at 100. `code_outline`, `code_symbols`,
  `code_grep`, `code_refs` (paginated relations; `definition` takes no `limit`), `find_files` and a
  `memory_search` listing cap at 1000 rows (`find_files` 5000) and set `limit_clamped: true`. Ask at
  or below the maximum and follow `next_cursor`.
- **Cursors are bound to their query.** A cursor continues only the call that returned it: never
  reuse one across queries (`code_symbols` cannot detect it and skips results). `code_symbols`,
  `code_grep` and `find_files` cursors are keyed to the index generation (a rescan or index change
  moves it, a HEAD move alone does not); `git_log` cursors to HEAD. On a mismatch the tool returns
  an error or an empty page with `cursor_invalidated: true`: repeat without `cursor`.
- **Notices.** An empty or partial answer with a `notice` (`empty_index`, `warming_up`,
  `building_index`, `rescanning`) means retry shortly, not "no matches".
- **Text answers** (ADR-0053). `code_grep` writes other fields as `key: value` lines, then each file as `# <path>` and each
  enclosing symbol as `## <symbol>` over its rows; rows before a symbol header or under a bare
  `##` are outside every symbol. `code_outline`, `git_diff` and a `find_files` list are
  [TOON](https://toonformat.dev): `key: value` lines, and a list of objects as one table,
  `symbols[N]{name,kind,...}:` plus a comma-separated row per object. Every other tool answers in
  compact JSON. MCP `structuredContent` is always JSON.
- **CLI output.** Human output prints bodies and diffs in full and cuts only table cells.
  `--json` prints the MCP `structuredContent` JSON.

## Truncation reasons

`truncated_reason` says why an answer is incomplete.

- `scan_cap`, `code_symbols`, `code_refs`, `memory_search`, `impact`: `total` is a lower bound: follow `next_cursor`, else narrow with `path` or `language`.
- `byte_budget`, `code_grep`: Follow `next_cursor`, or narrow with `path` or `language`.
- `time_budget`, `code_grep`: Sweep stopped at 10 s (see `notice`): follow `next_cursor`, or narrow.
- `depth`, `code_search`, `docs_search`, `impact`: Past 100 hits, or an import walk at its depth: refine the query.
- `liveness_depth`, `docs_search`: Narrow with `path`, `keywords` or `format`.
- `stale_chunks`, `code_search`: Hits in files edited since indexing were dropped (`omitted`): `index_rescan` them.
- `imports`, `code_outline`: Import list cut at 200: `code_grep` for the import.
- `size_cap`, `code_read`: read a span with `line` and `end_line`.
- `too_large`, `git_blame`: Blame a span or one `symbol`.
- `hunk_cap`, `byte_cap`, `git_diff`: stopped at 200 hunks or 128 KiB, no cursor: narrow `from`/`to`.
- `path_cap`, `git_diff`: Status list cut (`omitted`): pass a directory `path` ending in `/`.
- `span_limit`, `git_log`: Follow `next_cursor`, an empty page included, until it is absent.
- `unindexed_scan`, `git_log`: No history index, newest commits only: wait for it (`index_status`), repeat.
- `commit_cap`, `basemind git hotspots`: The window holds more commits than the walk reads: narrow `since`.
- `shallow_clone`, `git_log`, `git_blame`: An absent commit is inconclusive: `git fetch --unshallow`.
