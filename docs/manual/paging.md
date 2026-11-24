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
- **Limits are stated, not silent.** Each `limit` argument's description gives its default and
  maximum; a larger value is capped to the maximum. `find_files` also sets `limit_clamped`.
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

`truncated_reason` says why an answer is incomplete; the next step is what to do about it.

| Reason | Tool | Meaning | Next step |
| --- | --- | --- | --- |
| `scan_cap` | `code_symbols`, `code_refs`, `memory_search`, `impact` | The scan stopped at its cap; `total` is a lower bound. | Follow `next_cursor` where there is one; else narrow with `path` or `language`. |
| `byte_budget` | `code_grep` | The corpus is larger than one call reads. | Narrow with `path` or `language`, or follow `next_cursor`. |
| `depth` | `code_search`, `docs_search`, `impact` | A ranking goes on past 100 hits, or an import walk stopped at its depth. | Refine the query, or start the walk nearer the target. |
| `liveness_depth` | `docs_search` | A search lane hit its candidate depth before enough live documents were found. | Narrow with `path`, `keywords` or `format`. |
| `stale_chunks` | `code_search` | Hits in files edited since indexing were dropped; `omitted` counts them. | `index_rescan` the edited files, or repeat the call. |
| `imports` | `code_outline` | The import list was cut at 200. | `code_grep` for the import you need. |
| `size_cap` | `code_read` | The body was cut at the read cap. | Read a span with `line` and `end_line`. |
| `too_large` | `git_blame` | The file is too large to blame whole; the answer is empty. | Blame a span (`line`, `end_line`) or one `symbol`. |
| `hunk_cap`, `byte_cap` | `git_diff` | The diff stopped at 200 hunks or 128 KiB; it has no cursor. | Narrow `from` and `to`. |
| `path_cap` | `git_diff` (no `from`/`to`) | The status list was cut; `omitted` counts the paths left out. | Pass a directory `path` ending in `/`. |
| `span_limit` | `git_log` with a directory or symbol `path` | The page used up its span of commits before filling; older commits were not inspected. A directory page reports `commits_reached` and, when the git-history index serves it, `total_commits`. | Follow `next_cursor`, an empty page included, until it is absent; stop when `commits_reached` is far enough back. |
| `commit_cap` | `basemind git hotspots` (CLI only) | The walk stopped at its commit cap (20,000 indexed, 1,000 live). | Shorten `since`. |
| `unindexed_scan` | `git_log` with `query` or `author` | The git-history index could not serve the search, so only the newest commits were scanned. | Wait for the index (`index_status`), then repeat. |
| `shallow_clone` | `git_log`, `git_blame` | The clone is shallow: an absent commit is inconclusive. | `git fetch --unshallow`, then repeat. |
