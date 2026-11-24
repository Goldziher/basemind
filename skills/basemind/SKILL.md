---
description: Navigate large or unfamiliar codebases via the basemind MCP server — outlines, symbol search, reference/caller lookups, commit history, blame, and diffs without reading source files. Reach for it whenever the user asks "where is X defined", "what calls Y", "what changed recently in Z", or whenever you're about to grep or open many files to find structural information.
name: basemind
license: MIT
metadata:
    author: Goldziher
    repository: https://github.com/Goldziher/basemind
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:60734c8b746ff3ee862211ab7febcba3d91360cdb5fbda8ecae0343a4cb01608
Source-Hash: blake3:e320d694b0258ed06394743c75847d5990c0235672b6db89060dbf11b321b37d
Schema-Version: v1
-->

# basemind — the indexed context layer

basemind is the context layer for this repository, served over MCP. It pre-indexes the repo into a
content-addressed blob store + Fjall inverted index (and, when enabled, a LanceDB vector store) so
structural, historical, and semantic questions resolve in milliseconds without reading whole files.

Each job is its own tool with an exact schema. The job tools are always loaded: call them directly,
no tool search needed. The agent tools `agent_post`, `agent_inbox` and `agent_threads` carry no
always-load marker, so a host that defers MCP tools defers them; they are listed separately and
needed only when collaborating.

## Capabilities

- **Code map across 300+ languages**: tree-sitter outlines, symbol search, call sites, callers,
  implementations, importers. The code map (outline, symbols, refs) covers code only.
- **Exact text search**: `code_grep` (literal or regex) over code and markdown, config and data
  files.
- **Search by meaning**: `code_search` over code; `docs_search` over documents, including PDFs,
  Office files, HTML and email (images too, when the build has `documents-ocr`), with cross-encoder
  reranking. For exact text in an indexed HTML file, `code_grep` reads it as text too.
- **Git history**: log, blame, and diffs at symbol resolution.
- **Shared memory**: per-repo notes that persist across sessions (`memory_save`, `memory_search`).

## Dedicated per-capability skills

This umbrella skill covers the whole surface. For focused workflows:

- **`basemind-code-search`**: outlines, symbol search, usages, grep, search by meaning.
- **`basemind-git-history`**: log, blame, diffs.
- **`basemind-documents`**: document search and memory.
- **`basemind-comms`**: coordinating with other agents in the same repo (the deferred `agent_post`, `agent_inbox` and
  `agent_threads` tools).
- **`basemind-cli`**: the same surface driven headlessly from the CLI.

## When to reach for it (instead of `grep` / `read_file`)

- **Locating a symbol**: "where is `Foo` defined?", "find the constructor for `Bar`".
- **Following callers**: "what calls `process_file`?", "what imports this module?".
- **Mapping a file's shape** before reading it: which symbols, in what order, with what signatures.
- **Walking recent history**: "what changed in this file in the last 20 commits?", "when did this
  symbol last change?".
- **Blame and ownership**: "who last touched this function?", "what commit introduced this line?".
- **Diffing across revisions**: "what symbols did this branch add?", "show the hunks for `foo.rs`
  between HEAD~5 and HEAD".

If you are about to open more than two or three files just to learn structure, stop and use
basemind first. The tools return paths and line numbers; read source only once you know the span.

## Context economy

basemind tools return **paths, 1-based line numbers, and signatures, not file bodies**, so a
structural answer costs a fraction of the tokens of reading source.

- **`code_outline` before you open a file.** Then `code_read` the one symbol or line span you need
  instead of the whole file.
- **`code_symbols` instead of `grep`/`rg` for a definition.** It matches indexed symbol names and
  skips the comment, string and test-name noise.
- **`code_refs` instead of grepping call sites.** Indexed call edges, not text matches.
- **`code_grep` instead of shelling out to ripgrep.** The pattern is literal text unless you pass
  `"regex":true`; every hit carries its line and the enclosing symbol.
- **The index follows your edits**: no rescan and no server reconnect after each one. If a result
  says `stale` or a new file is missing, run `index_rescan {"paths":["src/store.rs"]}` for the files
  you touched; run `index_rescan {}` only when `index_status` reports an empty index, `degraded` or
  `rescan_needed`. Files under `scan.extra_roots` are the exception: the watcher does not track them
  and a path-scoped rescan skips them, so a new, changed or deleted file there needs a full pass:
  `index_rescan {}` while a server holds the lock, else `basemind scan`.
- **Do not re-read a file basemind already mapped.** If the outline answered the question, stop.

Drop to raw shell, grep, or git only when no basemind tool covers the question.

## Tool routing

| Question | Call |
|---|---|
| "What's the shape of file F?" | `code_outline {"path":"src/store.rs"}` |
| "Where is X defined?" | `code_symbols {"name":"parse_config","kind":"function"}` |
| "I only half remember the name" | `code_symbols {"name":"prsconf","fuzzy":true}` |
| "Show me this one symbol's body" | `code_read {"path":"src/store.rs","symbol":"open"}` |
| "Show me these lines" | `code_read {"path":"src/store.rs","line":120,"end_line":160}` |
| "Where does this string appear?" | `code_grep {"pattern":"cache_root(","path":"src/"}` |
| "Regex over code?" | `code_grep {"pattern":"fn \\w+_call","regex":true}` |
| "Find code by meaning, not spelling" | `code_search {"query":"retry a failed upload with backoff"}` |
| "What calls X?" (any definition of the name) | `code_refs {"name":"open_store"}` |
| "What calls anything named like X?" | `code_refs {"name":"open_","match":"substring"}` |
| "What calls this specific definition?" | `code_refs {"name":"open_store","path":"src/store.rs"}` |
| "What implements / extends X?" | `code_refs {"name":"Drawable","relation":"implementations"}` |
| "What imports module M?" | `code_refs {"name":"tokio::sync","relation":"importers"}` |
| "Which definition does this reference bind to?" | `code_refs {"name":"open_store","relation":"definition","path":"src/main.rs","line":42}` |
| "Which file is named like X?" | `find_files {"query":"readme"}` |
| "What files are indexed under a directory?" | `find_files {"path":"src/mcp/"}` |
| "What changed recently?" | `git_log {}` |
| "Which commits touched file F?" | `git_log {"path":"src/store.rs","limit":5}` |
| "Which commit mentions X?" | `git_log {"query":"fix race"}` |
| "When did symbol X change?" | `git_log {"path":"src/store.rs","symbol":"open"}` |
| "Show one commit" | `git_log {"rev":"HEAD~1"}` |
| "Who wrote this symbol?" | `git_blame {"path":"src/store.rs","symbol":"open"}` |
| "What's dirty in the working tree?" | `git_diff {}` |
| "What did I change in file F?" | `git_diff {"path":"src/store.rs"}` |
| "Diff file F between revisions" | `git_diff {"path":"src/store.rs","from":"HEAD~1","to":"HEAD"}` |
| "Where is this key in a config, markdown or data file?" | `code_grep {"pattern":"timeout:"}` |
| "Where is a topic discussed? Search PDFs / Office / HTML / email" | `docs_search {"query":"how releases are versioned"}` |
| "Remember this for future sessions" | `memory_save {"key":"decision/auth-tokens","value":"Tokens rotate every 24h.","tags":["auth"]}` |
| "Recall something stored earlier" | `memory_search {"query":"token rotation"}` |
| "What's indexed? What's HEAD / branch?" | `index_status {}` |
| "A result says stale, or a new file is missing" | `index_rescan {"paths":["src/store.rs"]}` |
| "Fetch the next page" | pass the response's `next_cursor` back as `cursor` |

Churn ranking, regex path filters over history, deleting a note, cache administration and the usage
summary are CLI only: `basemind git churn`, `basemind git by-path <regex>`,
`basemind memory delete <key>`, `basemind cache stats|gc|clear`, `basemind admin telemetry`.

## Setup (one-time per repo)

basemind needs an index before it can answer queries. The index lives in a machine-global cache
(Linux `~/.local/share/basemind/`, macOS `~/Library/Application Support/basemind/`; override
`BASEMIND_DATA_HOME`), keyed by workspace, never inside your repo. From the repo root:

```sh
basemind scan
```

This walks the tree, parses with tree-sitter, and writes a content-addressed blob store + Fjall
inverted index into the machine-global cache. A few seconds for small repos, ~22 s for an ~80k-file
TypeScript monorepo.

The MCP server is launched by the host (`basemind serve`, wired up in `.claude-plugin/plugin.json`
for you). You do not start it manually.

The server watches the workspace and keeps the index fresh as you edit. A tool that reports an empty
index says so in its result: run `index_rescan {}` or `basemind scan`.

## Examples

### Locating a symbol

```text
code_symbols {"name":"MapCache"}
→ {"results":["src/engine/map_cache.rs:10: pub(crate) struct MapCache",
              "src/engine/map_cache.rs:39: impl MapCache"], …}
```

A row is `"<path>:<line>: <signature>"`, as `git grep -n` prints it.

### Following call sites

```text
code_refs {"name":"process_file"}
→ {"total":1,"hits":{"src/scanner_file.rs":[{"line":113}]}, …}
```

`hits` maps each file to its call sites; a row adds `callee` when the name called differs, and
`resolution` when its grade is not the page's `resolution_default`.

### Outline a file, then read one body

```text
code_outline {"path":"src/wire.rs"}
→ path: src/wire.rs
  language: rust
  …
  symbols[…]{name,kind,line,end_line,signature}:
    encode,function,31,…
    toon,function,42,…
    …

code_read {"path":"src/wire.rs","symbol":"toon"}
```

The outline is a TOON table, one row per symbol.

A 1000-line file becomes a 30-line table of contents.

## Notes

- All paths are repository-relative with forward-slash separators. `line`, `end_line` and `column`
  are 1-based; `end_line` is inclusive.
- An argument a tool does not take is rejected with a did-you-mean and the accepted list, never
  silently ignored. A failure the caller can fix is a tool result with `isError` that names the fix.
- Every list takes `limit` and `cursor` and returns `next_cursor`. A list is trimmed to a default
  budget of 6000 tokens and then sets `budgeted`: raise `max_tokens` or follow `next_cursor`.
  Defaults: `code_symbols` / `code_grep` / `code_refs` 100, `find_files` 200, `code_outline` 500
  symbols, `code_search` / `docs_search` 10, `git_log` 20.
- `code_symbols` ranks the exact name first, then prefix, then substring; `"fuzzy":true` adds fuzzy
  subsequence matches last. A signature without the name is written `<kind> <name>; <signature>`.
  `total` appears only beside `next_cursor`.
- `code_refs {"name":"bar"}` matches the identifier exactly, with no scope resolution: `Foo::bar()`
  and `bar()` are calls to `bar`, `bar_baz()` is not. `"match":"substring"` widens it. Add `path` to
  pin one definition: the response names that `definition`, and `resolved_total` counts the hits
  proven to bind to it. Each row is graded by `resolution` (`same_file`, `imported`,
  `scope_resolved`, `elsewhere`, `name_only`, `external`, `path_match`); a row leaves it out when it
  is the page's `resolution_default`, and a page leaves that out when it is `name_only`.
  `resolution_note` says why no hit could be checked (`unsupported for <language>`,
  `no resolution facts for this file`, `no resolution facts for this definition`). A missing file
  or symbol is an error, and so is a name that several definitions in the file share, until you
  add `line` (any line inside the one you mean). A pinned file edited since it was indexed is an
  error naming `index_rescan`.
- `code_search` reports which retrieval lanes ran (`lanes_ran`) and which could not, with the
  reason (`lanes_unavailable`).
- `path` on `code_grep`, `find_files`, `code_symbols`, `code_search` and `docs_search` is a repo-relative prefix (`src/mcp/`), or a
  glob over the whole path when it holds `*` (`src/**/*.rs`); `mcp/` is not a prefix of `src/mcp/`.
- A cursor continues only the call that returned it, with the same arguments apart from `limit`
  and `max_tokens`. On another query, or after a rescan or a new commit, the call is an error:
  repeat it without `cursor`.
- A scalar sent as a string (`"limit":"10"`, `"regex":"true"`) is accepted; any other type error
  names the field.
- The code map (`code_outline`, `code_symbols`, `code_refs`) covers code only. `code_grep` also
  finds exact text in markdown, config and data files; `docs_search` is for searching by meaning,
  and for PDFs, Office files, HTML and email. `find_files` lists the indexed files and, in a git
  work tree, the other files git tracks; it leaves out an untracked file the index excludes (a local
  `.env`). `code_read` reads an indexed file only; read one the scan excludes with the host's file
  tools.
- A `notice` field means the result is incomplete and says why (empty index, scan in progress).
- The git tools require `basemind serve` to be running inside a git repository.
- `code_search` needs the `code-search` Cargo feature, `docs_search` needs `documents`, and the
  memory tools need `memory` (all in `--features full` and in the release binary). Without the
  feature the tool answers with an error naming it.
- Memory is one store per repository, keyed by the path of its main worktree: its worktrees
  share notes (per worktree when it has no `origin`); another clone, a moved repository or an
  `origin` that normalises differently does not see them, and a re-clone at the same path with
  the same `origin` does.
- MCP resources are available for hosts that attach them: `basemind://status`,
  `basemind://outline/{path}` and `basemind://memory/{key}`.
