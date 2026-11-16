---
type: Playbook
title: Basemind Code Search
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
  not both. A body over 128 KiB is cut after a whole line and flagged `truncated`
  (`omitted_bytes` is what was left out): read on from `end_line + 1`. `stale: true` (also on `code_outline`) means the file changed since it was
  indexed and was read as it is now; a file deleted since the scan is an error naming
  `index_rescan`.
- `code_outline` omits `imports` when the file has none.
- `code_grep`: `pattern` is literal text unless `"regex":true`, and a regex anchors `^` / `$` at
  every line. `path` is a substring of the repo-relative path, never a glob; a `path` or
  `language` that no indexed code file has is an error (it names `docs_search` when only
  documents match). `skipped_files` counts files that could not be searched. A hit is `path`, `line`, `column`, `text` (the matching line; one
  over 400 bytes is cut to a window around the match, `…` marking each cut side) and `symbol`
  (`name`, `kind` of the innermost enclosing symbol; absent, with `stale: true`, for a file edited
  since it was indexed);
  `"context":true` adds `context_before` / `context_after`. The response echoes `pattern` and
  `regex`. It never caps files, so `total_matches` is exact.
- `code_symbols` ranks the exact name first, then prefix, then substring; `"fuzzy":true` adds fuzzy
  subsequence matches last. A row is `path`, `name`, `kind`, `line`, `column`, `signature` and
  `match` (`exact`, `prefix`, `substring` or `fuzzy`); `total` counts the whole corpus.
- `code_refs {"name":"bar"}` matches the identifier exactly, with no scope resolution: `Foo::bar()`
  and `bar()` are calls to `bar`, `bar_baz()` is not. `"match":"substring"` widens it, and the
  response echoes `match`. Add `path` for one specific definition: the response carries that
  `definition` (`path`, `name`, `kind`, `line`, `column`) and either `resolved_total` with
  `resolved` on the hits it checked (`true`: proven to bind to the definition; `false`: checked
  and not proven; absent: not checked), or a `resolution` note with no flags:
  `unsupported for <language>`, `no resolution facts for this file` when the language is supported
  but the file bound nothing, or `no resolution facts for this definition`. A pinned file edited
  since it was indexed is an error naming `index_rescan`.
  A `path` that is not indexed, or that defines no such symbol, is an error; so is a name several
  definitions in the file share, which lists each with its lines until you add `line`.
  `relation:"importers"` matches whole segments of recorded imports (`os` finds `import os.path`,
  not `import posixpath`) and takes no `match`.
  `relation:"definition"` needs `path` and `line` and returns the reference's `path`, `line`,
  `column` plus the `definition` it binds to (or a `resolution` note), all 1-based.
- `code_search` returns pointers (path, `line` / `end_line`, symbol), not bodies; read a hit with
  `code_read`. `lane` picks `hybrid` (default), `semantic` or `keyword`. The response lists
  `lanes_ran` (`vector`, `keyword`, `exact`) and `lanes_unavailable` (`lane`, `reason`), so a hybrid
  search that ran without embeddings says so. It needs the `code-search` Cargo feature.
- Every list takes `limit` and `cursor` and returns `next_cursor`. Defaults: `code_symbols` /
  `code_grep` / `code_refs` 100, `find_files` 200, `code_outline` 500 symbols, `code_search` 10.
  A list is trimmed to a default budget of 6000 tokens and then sets `budgeted`: raise `max_tokens`
  or follow `next_cursor`. A page that ended at `limit` sets nothing else: `next_cursor` is the
  signal that there is more. `truncated` + `truncated_reason` mark a cut you did not ask for, such
  as `scan_cap` on `code_refs` / `code_symbols` (the scan stopped early, `total` is a lower bound,
  `next_cursor` resumes it) or `byte_budget` on `code_grep` (files left unread).
- A cursor continues only the call that returned it, with the same arguments apart from `limit`
  and `max_tokens`. Passed to another query, or used after a rescan, the call is an error: repeat
  it without `cursor`.
- A `notice` (`empty_index`, `warming_up`, `building_index`, `rescanning`) means results may be
  incomplete or a moment stale.
- Needs an index in the machine-global cache (Linux `~/.local/share/basemind/`, macOS
  `~/Library/Application Support/basemind/`; override `BASEMIND_DATA_HOME`). Run `basemind scan`
  first (see the `basemind-scan` skill).

For git history, blame and diffs see `basemind-git-history`; for document search see
`basemind-documents`; for agent coordination see `basemind-comms`.
