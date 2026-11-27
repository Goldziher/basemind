---
type: Playbook
title: basemind code search
description: Find where code is defined and used without reading files — symbol search,
  file outlines, references, callers, implementations, dependents, and
  indexed regex over content. Reach for it whenever the user asks "where is X defined",
  "what calls Y", "what implements Z", "what's the shape of this file", or whenever
  you're about to grep or open files to learn structure.
x-ai-rulez:
  kind: skill
  id: basemind-code-search
  metadata:
    name: basemind-code-search
    license: MIT
    metadata:
      author: Goldziher
      repository: https://github.com/Goldziher/basemind
---

# basemind-code-search — navigate code without reading it

basemind pre-indexes the repo into a tree-sitter code map across 300+ languages. The code map
(outline, symbols, refs) covers **code only**. `code_grep` also finds exact text in markdown,
config and data files (Markdown, JSON, YAML, TOML, XML, CSV, INI). `docs_search` is for searching
by meaning, and for PDFs, Office files, HTML and email (see `basemind-documents`). `find_files`
lists the indexed files and the other files git tracks, not an untracked file the index leaves out.

Structural questions (where a symbol lives, what calls it, what shape a file has) resolve from the
index in milliseconds and return **paths, 1-based line numbers, and signatures, not file bodies**.

The tools are always loaded: call them directly. Drop to raw shell only when no tool covers the question.

## The discipline

- **`code_outline` before you open a file.** A 1000-line file becomes a 30-line table of contents.
  Then `code_read` the one symbol or line span you need, not the file.
- **`code_symbols` instead of `grep` for a definition.** It matches indexed symbol names and skips
  the comment, string and test-name noise.
- **`code_refs` instead of grepping call sites.** Indexed call edges, not text matches.
- **`code_grep` instead of shelling out to ripgrep.** It sweeps every indexed code, markdown,
  config and data file (a trigram filter skips code files that cannot match) and returns each hit's
  line and enclosing symbol. A file created since the last scan is not in it until the watcher or
  an `index_rescan {"paths":[…]}` indexes it, so a miss there is not proof of absence.
- **Do not re-read a mapped file** once the outline answered the question.
- **The index follows your edits** while a server, daemon or `basemind watch` runs: no rescan and
  no server reconnect after each one. If a result says `stale` or a new file is missing, run
  `index_rescan {"paths":["src/store.rs"]}` for that file; run `index_rescan {}` only when
  `index_status` reports an empty index, `degraded` or `rescan_needed`. Files under
  `scan.extra_roots` are not watched and `paths` skips them: refresh a changed one with a full pass
  (`index_rescan {}`, or `basemind scan` with no server). With only a standalone `basemind watch`
  there is no `index_rescan` and `basemind rescan` exits 3 on its lock: stop the watcher, rescan,
  restart it.
- **CLI only, with nothing watching**: the index does not follow edits and shows no `stale` flag.
  Run `basemind rescan <path>…` for the files you edited before querying them.

## Tool routing

| Question | MCP call | CLI |
|---|---|---|
| "What's the shape of file F?" | `code_outline {"path":"src/store.rs"}` | `basemind code outline src/store.rs` |
| "Where is X defined?" | `code_symbols {"name":"parse_config","kind":"function"}` | `basemind code symbols parse_config --kind function` |
| "Where is X defined in this directory?" | `code_symbols {"name":"Config","path":"src/mcp/"}` | `basemind code symbols Config --path src/mcp/` |
| "I only half remember the name" | `code_symbols {"name":"prsconf","fuzzy":true}` | `basemind code symbols prsconf --fuzzy` |
| "Show one symbol's body" | `code_read {"path":"src/store.rs","symbol":"open"}` | `basemind code read src/store.rs --symbol open` |
| "Show these lines" | `code_read {"path":"src/store.rs","line":120,"end_line":160}` | `basemind code read src/store.rs --line 120 --end-line 160` |
| "Where does this text appear?" | `code_grep {"pattern":"cache_root(","path":"src/"}` | `basemind code grep 'cache_root(' --path src/` |
| "Regex over code?" | `code_grep {"pattern":"fn \\w+_call","regex":true}` | `basemind code grep 'fn \w+_call' --regex` |
| "Find code by meaning?" | `code_search {"query":"retry a failed upload with backoff"}` | `basemind code search "retry a failed upload with backoff"` |
| "What calls X?" (any definition of the name) | `code_refs {"name":"open_store"}` | `basemind code refs open_store` |
| "What calls anything named like X?" | `code_refs {"name":"open_","match":"substring"}` | `basemind code refs open_ --match substring` |
| "What calls this specific definition?" | `code_refs {"name":"open_store","path":"src/store.rs"}` | `basemind code callers src/store.rs open_store` |
| "What implements / extends / inherits X?" | `code_refs {"name":"Drawable","relation":"implementations"}` | `basemind code implementations Drawable` |
| "What imports module M?" | `code_refs {"name":"tokio::sync","relation":"importers"}` | `basemind code dependents tokio::sync` |
| "Which definition does this reference bind to?" | `code_refs {"name":"open_store","relation":"definition","path":"src/main.rs","line":42}` | `basemind code definition src/main.rs 42 open_store` |
| "Which file is named like X?" | `find_files {"query":"readme"}` | `basemind code find readme` |
| "What files are indexed under a directory?" | `find_files {"path":"src/mcp/"}` | `basemind code files --path src/mcp/` |
| "A result says stale, or a new file is missing?" | `index_rescan {"paths":["src/store.rs"]}` | `basemind rescan src/store.rs` |

## Notes

- An argument a tool does not take is rejected with a did-you-mean and the accepted list. A fixable
  failure is a tool result with `isError` naming the fix: `docs_search` for a document path,
  `index_rescan` for a file that is not indexed, `find_files` for a missing one.
- `path` is repo-relative; `line`, `end_line` and `column` are 1-based.
- `code_read` takes `symbol` (add `kind` when several share the name) or `line` with `end_line`,
  not both. A `code_read` body over 128 KiB is cut at a line end and flagged `truncated`
  (`omitted_bytes` is what was left out): read on from `end_line + 1`. `stale: true` (also on `code_outline`) means the file
  changed since it was indexed and was read as it is now; a deleted file is an error naming
  `index_rescan`.
- `code_grep`: `pattern` is literal text unless `"regex":true`, and a regex anchors `^` / `$` at
  every line. `path` is a substring of the repo-relative path, never a glob; a `path` or `language`
  that no indexed code or text file has is an error (it names `docs_search` when only PDFs, Office
  files or other binary documents match).
  `skipped_files` counts files that could not be searched; `skipped_reasons` splits them into
  `unavailable`, `too_large` and `unreadable`. The answer is `key: value` lines, then
  each file as `# <path>` and each innermost enclosing symbol as `## <symbol>` over its rows
  `<line>: <text>` (rows before any symbol header are outside every symbol); matches sharing a
  line are one row `<line>:<col>,<col>: <text>`, and a clipped line (over 400 bytes) writes its
  column `<line>:<col>: ...`. `"context":N` (0-10) adds the N lines around each hit as rows
  `<line>- <text>`. `stale` lists files edited since they were indexed. `total` appears only
  beside `next_cursor`.
- `find_files` lists each file by its path alone: the code map, the documents, and every other
  file git tracks (images, fonts, data). A query no path holds as typed may be a typo: a file it
  names up to one swapped, missing, extra or wrong character comes first.
- `code_symbols` ranks the exact name first, then prefix, then substring; `"fuzzy":true` adds fuzzy
  subsequence matches last. A row is `"<path>:<line>: <signature>"`, in rank order; a signature
  lacking the name, or none, is written `"<path>:<line>: <kind> <name>[; <signature>]"`. `stale` lists files
  edited since the scan; `total` appears only beside `next_cursor`.
- `code_refs {"name":"bar"}` matches the identifier exactly: `Foo::bar()` and `bar()` are calls to
  `bar`, `bar_baz()` is not. `"match":"substring"` widens it. `hits` lists each file once, as
  `{"<path>": [hit, ...]}`; a call site is its `line` (and `column` when another call of the same
  callee is on that line), plus `callee` when that is not `name`, and an implementation is its
  `line` and `impl_type`. Each call site is graded by `resolution` (see Resolution below). A
  `path` that is not indexed, or defines no such symbol, is an error; so is a name several
  definitions in the file share, which lists each with its lines until you add `line`.
  `relation:"importers"` matches whole segments of recorded imports (`os` finds `import os.path`,
  not `import posixpath`) and takes no `match`. With `path`, `in_cycle` counts the listed importers
  that share an import cycle with the file, and only `scan_cap` truncates (a walk stopped at `depth`
  does not); `deeper: true` says more importers lie past `depth` (default 1, at most 5).
  `total` appears only beside `next_cursor`.
- `code_search` returns pointers (path, `line` / `end_line`, symbol), not bodies; read a hit with
  `code_read`. `lane` picks `hybrid` (default), `semantic` or `keyword`. The response lists
  `lanes_ran` and `lanes_unavailable` (`lane`, `reason`), so a hybrid search that ran without
  embeddings says so. It needs the `code-search` Cargo feature.
- Needs an index in the machine-global cache (Linux `~/.local/share/basemind/`, macOS
  `~/Library/Application Support/basemind/`; override `BASEMIND_DATA_HOME`). Run `basemind scan`
  first (see the `basemind-scan` skill).

## Resolution

<!-- manual:resolution -->
`code_refs` finds call sites by name, then grades each one by what it is known to bind to. The
grade refines a result set; it never shrinks it. A call without `resolution` lists every call site
whose callee is the name, graded, so `total` stays a sound count of candidates.

Each call site carries `resolution`, one of:

| Value | Meaning |
|---|---|
| `same_file` | Binds to a definition in the same file. |
| `imported` | Binds through an import to a definition in another file. |
| `scope_resolved` | Binds to a local binding that is not an indexed definition: a variable, a parameter, or an import whose target is not indexed. |
| `elsewhere` | Checked against the definition pinned with `path`, and binds to another one. |
| `name_only` | No resolution facts for the site: matched by name alone. |
| `external` | No resolution facts, and the index defines no symbol of the name. |
| `path_match` | An HTTP call linked to a route by its method and normalized path. |

A page names its most common grade once as `resolution_default`, and a row leaves its own out when
it is that one; a page with no `resolution_default` is all `name_only`. `resolution_counts` gives
the rows per grade, before any filter.

- **Callers of one definition.** Add `path` (and `line` when several definitions in the file share
  the name): the answer carries that `definition`, and `resolved_total` counts the hits proven to
  bind to it. When resolution could not run, `resolved_total` is absent and `resolution_note` says
  why: `unsupported for <language>`, `no resolution facts for this file`, or
  `no resolution facts for this definition`.
- **Precision on request.** `resolution` filters rows by grade, comma-separated:
  `{"name":"open","resolution":"same_file,imported"}` (CLI: `--resolution same_file,imported`).
- **Why the default keeps unresolved rows.** Resolution cannot see through a module-object import
  (`from pkg import mod`, then `mod.f()`) or an unresolvable path alias. Dropping those rows would
  lose real callers and report the rest as complete. Filter when you want precision; trust `total`
  when you need completeness.
- **Coverage.** JavaScript and TypeScript (oxc), Python and Java (stack-graphs rules) resolve
  across files through imports; every other language gets tree-sitter scope binding.
- **JS/TS workspaces.** A bare import of a workspace package (`@acme/ui`, `@acme/ui/button`)
  resolves to its source with no `node_modules`. Packages come from the root `pnpm-workspace.yaml`
  `packages:` globs and the root `package.json` `workspaces` field, each match with a named
  `package.json`. A symlink into the repository resolves to its real path, and a literal
  `require('./x')` is a file edge.
- **From a reference to its definition.** `relation: "definition"` returns the definition the
  reference at `path` / `line` binds to, or a `resolution_note` when there is none.
- **HTTP routes.** A route is a `route` symbol named `METHOD /path`, parameters written `:param`;
  a call with a literal URL is a call site of the same shape. The callers of a route (its name, or
  `path` pinning it) are the calls it matches best, graded `path_match`: the methods agree (a route
  of `ANY` takes every method) and the paths match segment for segment, else one is a suffix of the
  other (a client base URL, or a router mounted in another file). A match through a literal segment
  beats one through parameters and wildcards alone (a catch-all such as `GET /*` never takes a call
  from the route naming its path), an exact match a suffix, a longer suffix a shorter one, and a
  literal segment a parameter. A call that several routes match equally well carries `ambiguous`:
  two services may expose the path. The callers of a handler (`path` pinning it) add the routes
  naming it and their calls, `ambiguous` for a route in another file when more than one file
  defines the name. `relation: "definition"` on a call returns its route with
  `resolution: "path_match"`, or the tied routes under `ambiguous` and no `definition`. URLs built
  from variables are out of reach.
<!-- /manual:resolution -->

## Paging and budgets

<!-- manual:paging -->
Every list takes `limit` and `cursor` and returns `next_cursor` while more remains. Pass
`next_cursor` back as `cursor` (CLI: `--cursor`) with the same arguments; only `limit` and
`max_tokens` may change between pages.

- **Defaults.** `code_symbols`, `code_grep` and `code_refs` list 100 rows, `find_files` 200,
  `code_outline` 500 symbols, `code_search` and `docs_search` 10 hits, `git_log` 20 commits (at
  most 100), `git_blame` 100 hunks. `limit: 0` is refused.
- **Token budget.** A list is trimmed to `max_tokens` (default 6000) and then sets `budgeted`:
  raise `max_tokens` or follow `next_cursor`.
- **Three words for a cut.** A page that ended at `limit` returns only `next_cursor`. `budgeted`
  means the token budget trimmed it. `truncated` with `truncated_reason` marks a cut you did not
  ask for (`omitted` counts what was left out where known); see "Truncation reasons" below.
- **Limit cap.** A `limit` above its maximum (1000 rows; `find_files` 5000) is capped and the
  answer sets `limit_clamped: true` (absent when `limit` was kept; `code_outline`, `code_symbols`,
  `code_grep`, `code_refs`, `find_files` and a `memory_search` listing report it): ask at or
  below the maximum and follow `next_cursor`.
- **Cursors are bound to their query.** A cursor continues only the call that returned it. Passed
  to another query, or used after a rescan or HEAD move, it is an error, never an empty page:
  repeat the call without `cursor`.
- **Notices.** An empty or partial answer carrying a `notice` (`empty_index`, `warming_up`,
  `building_index`, `rescanning`) means retry shortly, not "no matches".
- **Text answers.** Text is written per tool in the form that measured fewest tokens (ADR-0053).
  `code_grep` writes its other fields as `key: value` lines, then each file as
  `# <path>` and each enclosing symbol as `## <symbol>` over its rows; rows before any symbol
  header, or under a bare `##`, are outside every symbol. `code_outline`, `git_diff` and a
  `find_files` list are [TOON](https://toonformat.dev): `key: value` lines, and a list of objects
  as one table, `symbols[N]{name,kind,...}:` and a comma-separated row per object, a value quoted
  only where it would be misread. Every other tool answers in compact JSON. A header naming a path
  or symbol that starts with `"`, has leading or trailing whitespace or holds a control character
  is a JSON string. MCP `structuredContent` is JSON for every tool.
- **CLI output.** Human output prints bodies and diffs in full and cuts only table cells, with a
  note on stderr. `--json` prints the response as JSON, as MCP `structuredContent` carries it.

### Truncation reasons

`truncated_reason` says why an answer is incomplete.

- `scan_cap`, `code_symbols`, `code_refs`, `memory_search`, `impact`: `total` is a lower bound: follow `next_cursor`, else narrow with `path` or `language`.
- `byte_budget`, `code_grep`: Follow `next_cursor`, or narrow with `path` or `language`.
- `time_budget`, `code_grep`: Sweep stopped at 10 s (see `notice`): follow `next_cursor`, or narrow.
- `depth`, `code_search`, `docs_search`, `impact`: Past 100 hits, or an import walk at its depth: refine the query.
- `liveness_depth`, `docs_search`: Narrow with `path`, `keywords` or `format`.
- `stale_chunks`, `code_search`: Hits in files edited since indexing were dropped (`omitted`): `index_rescan` them.
- `imports`, `code_outline`: Import list cut at 200: `code_grep` for the import.
- `size_cap`, `code_read`: Read a span with `line` and `end_line`.
- `too_large`, `git_blame`: Blame a span or one `symbol`.
- `hunk_cap`, `byte_cap`, `git_diff`: Stopped at 200 hunks or 128 KiB, no cursor: narrow `from` and `to`.
- `path_cap`, `git_diff`: Status list cut (`omitted`): pass a directory `path` ending in `/`.
- `span_limit`, `git_log`: Follow `next_cursor`, an empty page included, until it is absent.
- `unindexed_scan`, `git_log`: No history index, newest commits only: wait for it (`index_status`), repeat.
- `commit_cap`, `git_hotspots`: The window holds more commits than the walk reads: narrow `since`.
- `shallow_clone`, `git_log`, `git_blame`: An absent commit is inconclusive: `git fetch --unshallow`.
<!-- /manual:paging -->

For git history, blame and diffs see `basemind-git-history`; for document search see
`basemind-documents`; for agent coordination see `basemind-comms`.
