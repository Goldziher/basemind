---
description: Find where code is defined and used without reading files — symbol search, file outlines, references, callers, implementations, dependents, and indexed regex over content. Reach for it whenever the user asks "where is X defined", "what calls Y", "what implements Z", "what's the shape of this file", or whenever you're about to grep or open files to learn structure.
name: basemind-code-search
license: MIT
metadata:
    author: Goldziher
    repository: https://github.com/Goldziher/basemind
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:8119de89f6686713f6c6b78ae02eb616e9f10050d1fff43914c6a7882738f7f5
Source-Hash: blake3:9671938c539719311a274b3a1ddafa49af5b3c535d32416ee51e6b597b8751f3
Schema-Version: v1
-->

# basemind-code-search — navigate code without reading it

basemind pre-indexes the repo into a tree-sitter code map across 300+ languages. The map holds
**code only**. Jupyter notebooks (`.ipynb`), Markdown, JSON, YAML, TOML, XML, CSV, INI and `.env`
files are documents: `find_files` lists them, `docs_search` searches them by meaning (see
`basemind-documents`), and a literal or regex file search finds exact matches outside the code map.

Structural questions (where a symbol lives, what calls it, what shape a file has) resolve from the
index in milliseconds and return **paths, 1-based line numbers, and signatures, not file bodies**.

The tools are always loaded: call them directly, no tool search needed. Drop to raw shell only when
no tool covers the question.

## The discipline

- **`code_outline` before you open a file.** A 1000-line file becomes a 30-line table of contents.
  Then `code_read` the one symbol or line span you need, not the file.
- **`code_symbols` instead of `grep` for a definition.** It matches indexed symbol names and skips
  the comment, string and test-name noise.
- **`code_refs` instead of grepping call sites.** Indexed call edges, not text matches.
- **`code_grep` instead of shelling out to ripgrep.** It sweeps every indexed code file (a trigram
  filter skips files that cannot match) and returns each hit's line and enclosing symbol.
- **Do not re-read a file basemind already mapped.** If the outline answered the question, stop.
- **`index_rescan` after you edit code**, not a server reconnect. Pass `paths` to limit it.

## Tool routing

| Question | MCP call | CLI |
|---|---|---|
| "What's the shape of file F?" | `code_outline {"path":"src/store.rs"}` | `basemind code outline src/store.rs` |
| "Where is X defined?" | `code_symbols {"name":"parse_config","kind":"function"}` | `basemind code symbols parse_config --kind function` |
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
| "What files are indexed under a directory?" | `find_files {"path_prefix":"src/mcp/"}` | `basemind code files --path-prefix src/mcp/` |
| "What's indexed?" | `index_status {}` | `basemind status` |
| "Refresh the index after editing?" | `index_rescan {"paths":["src/store.rs"]}` | `basemind rescan src/store.rs` |
| "Fetch the next page?" | pass the response's `next_cursor` back as `cursor` | `--cursor <next_cursor>` |

## Examples

```text
code_symbols {"name":"MapCache"}
→ src/mcp/mod.rs:79 MapCache (struct)
  src/mcp/mod.rs:88 MapCache (impl)

code_refs {"name":"process_file"}
→ src/scanner.rs:142 process_file
  src/scanner.rs:201 process_file

code_outline {"path":"src/mcp/tools.rs"}
→ 31 code_outline (function)
  52 code_symbols (function)
```

## Notes

- An argument a tool does not take is rejected with a did-you-mean and the accepted list. A failure
  the caller can fix is a tool result with `isError` naming the fix: a path that is a document
  points at `docs_search`, a file that exists but is not indexed at `index_rescan`, a missing file
  at `find_files`.
- `path` is repo-relative with forward slashes. `line`, `end_line` and `column` are 1-based.
- `code_read` takes `symbol` (add `kind` when several share the name) or `line` with `end_line`,
  not both. A body over 128 KiB is cut at a line end (a single longer line inside it) and flagged `truncated`
  (`omitted_bytes` is what was left out): read on from `end_line + 1`. `stale: true` (also on `code_outline`) means the file changed since it was
  indexed and was read as it is now; a file deleted since the scan is an error naming
  `index_rescan`.
- `code_outline` omits `imports` when the file has none.
- `code_grep`: `pattern` is literal text unless `"regex":true`, and a regex anchors `^` / `$` at
  every line. `path` is a substring of the repo-relative path, never a glob; a `path` or `language`
  that no indexed code file has is an error (it names `docs_search` when only documents match).
  `skipped_files` counts files that could not be searched. The answer is `key: value` lines, then
  each file as `# <path>` and each innermost enclosing symbol as `## <symbol>` over its rows
  `<line>: <text>` (rows before any symbol header, or under a bare `##`, are outside every
  symbol); matches sharing a line are one row `<line>:<col>,<col>: <text>`, and a clipped line
  (over 400 bytes, cut to a window around the match) writes its column `<line>:<col>: ...`.
  `"context":N` (0-10) adds the N lines around each hit as rows `<line>- <text>`, each written
  once. `stale` lists files edited since they were indexed (their rows are outside every symbol).
  `total` appears only beside `next_cursor`.
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
  `lanes_ran` (`vector`, `keyword`, `exact`) and `lanes_unavailable` (`lane`, `reason`), so a hybrid
  search that ran without embeddings says so. It needs the `code-search` Cargo feature.
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
- **Three words for a cut.** A page that ended at `limit` returns `next_cursor` and nothing else.
  `budgeted` means the token budget trimmed it. `truncated` with `truncated_reason` marks a cut you
  did not ask for, with `omitted` counting what was left out where it is known: `scan_cap` (the
  scan stopped early, `total` is a lower bound, `next_cursor` resumes it), `byte_budget`,
  `hunk_cap`, `byte_cap`, `path_cap`, `window`, `live_walk` or `shallow_clone`.
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
<!-- /manual:paging -->

For git history, blame and diffs see `basemind-git-history`; for document search see
`basemind-documents`; for agent coordination see `basemind-comms`.
