---
type: Playbook
title: Basemind
description: Navigate large or unfamiliar codebases via the basemind MCP server —
  outlines, symbol search, reference/caller lookups, commit history, blame, and diffs
  without reading source files. Reach for it whenever the user asks "where is X defined",
  "what calls Y", "what changed recently in Z", or whenever you're about to grep or
  open many files to find structural information.
x-ai-rulez:
  kind: skill
  id: basemind
  metadata:
    name: basemind
    license: MIT
    metadata:
      author: Goldziher
      repository: https://github.com/Goldziher/basemind
---

# basemind — the indexed context layer

basemind is the context layer for this repository, served over MCP. It pre-indexes the repo into a
content-addressed blob store + Fjall inverted index (and, when enabled, a LanceDB vector store) so
structural, historical, and semantic questions resolve in milliseconds without reading whole files.

Each job is its own tool with an exact schema. The tools are always loaded: call them directly, no
tool search needed.

## Capabilities

- **Code map across 300+ languages**: tree-sitter outlines, symbol search, call sites, callers,
  implementations, importers. It holds **code only**: markdown, JSON, YAML, TOML, XML, CSV, INI and
  similar files are documents, searched with `docs_search`.
- **Text and meaning search over code**: `code_grep` (literal or regex) and `code_search`.
- **Git history**: log, blame, and diffs at symbol resolution.
- **Document search**: PDFs, Office, HTML, email, images (OCR, when the build has `documents-ocr`),
  markdown and config/data files, with cross-encoder reranking (`docs_search`).
- **Shared memory**: per-repo notes that persist across sessions (`memory_save`, `memory_search`).

## Dedicated per-capability skills

This umbrella skill covers the whole surface. For focused workflows:

- **`basemind-code-search`**: outlines, symbol search, usages, grep, search by meaning.
- **`basemind-git-history`**: log, blame, diffs.
- **`basemind-documents`**: document search and memory.
- **`basemind-comms`**: coordinating with other agents in the same repo over the broker.
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
- **`index_rescan` after you edit code**, not a server reconnect. Pass `paths` to limit it to the
  files you touched.
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
| "What files are indexed under a directory?" | `find_files {"path_prefix":"src/mcp/"}` |
| "What changed recently?" | `git_log {}` |
| "Which commits touched file F?" | `git_log {"path":"src/store.rs","limit":5}` |
| "Which commit mentions X?" | `git_log {"query":"fix race"}` |
| "When did symbol X change?" | `git_log {"path":"src/store.rs","symbol":"open"}` |
| "Show one commit" | `git_log {"rev":"HEAD~1"}` |
| "Who wrote this symbol?" | `git_blame {"path":"src/store.rs","symbol":"open"}` |
| "What's dirty in the working tree?" | `git_diff {}` |
| "What did I change in file F?" | `git_diff {"path":"src/store.rs"}` |
| "Diff file F between revisions" | `git_diff {"path":"src/store.rs","from":"HEAD~1","to":"HEAD"}` |
| "Search PDFs / markdown / config / data files" | `docs_search {"query":"how releases are versioned"}` |
| "Remember this for future sessions" | `memory_save {"key":"decision/auth-tokens","value":"Tokens rotate every 24h.","tags":["auth"]}` |
| "Recall something stored earlier" | `memory_search {"query":"token rotation"}` |
| "What's indexed? What's HEAD / branch?" | `index_status {}` |
| "Refresh the index after editing code" | `index_rescan {"paths":["src/store.rs"]}` |
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

Re-run `basemind scan` after large changes, or run `basemind watch` to keep the index fresh on file
save. A tool that reports an empty index says so in its result: run `index_rescan {}` or
`basemind scan`.

## Examples

### Locating a symbol

```text
code_symbols {"name":"MapCache"}
→ src/mcp/mod.rs:79 MapCache (struct)
  src/mcp/mod.rs:88 MapCache (impl)
```

### Following call sites

```text
code_refs {"name":"process_file"}
→ src/scanner.rs:142 process_file
  src/scanner.rs:201 process_file
```

### Outline a file, then read one body

```text
code_outline {"path":"src/mcp/tools.rs"}
→ 31 code_outline (function)
  52 code_symbols (function)
  ...

code_read {"path":"src/mcp/tools.rs","symbol":"code_outline"}
```

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
  subsequence matches last. Each row says which with `match`, and `total` counts the whole corpus.
- `code_refs {"name":"bar"}` matches the identifier exactly, with no scope resolution: `Foo::bar()`
  and `bar()` are calls to `bar`, `bar_baz()` is not. `"match":"substring"` widens it. Add `path` to
  pin one definition: the response names that `definition`, and a hit then has `resolved: true`
  (proven to bind to it), `resolved: false` (checked, not proven) or no `resolved` (not checked);
  `resolution` says why when no hit could be checked (`unsupported for <language>`,
  `no resolution facts for this file`, `no resolution facts for this definition`). A missing file
  or symbol is an error, and so is a name that several definitions in the file share, until you
  add `line` (any line inside the one you mean). A pinned file edited since it was indexed is an
  error naming `index_rescan`.
- `code_search` reports which retrieval lanes ran (`lanes_ran`) and which could not, with the
  reason (`lanes_unavailable`).
- `code_grep`'s `path` is a literal substring of the repo-relative path, never a glob.
- A cursor continues only the call that returned it, with the same arguments apart from `limit`
  and `max_tokens`. On another query, or after a rescan or a new commit, the call is an error:
  repeat it without `cursor`.
- A scalar sent as a string (`"limit":"10"`, `"regex":"true"`) is accepted; any other type error
  names the field.
- `code_outline`, `code_symbols` and `code_grep` cover code only. A markdown or config/data file is
  a document: `find_files` lists it, `docs_search` searches its contents, `code_read` with a line
  span reads it.
- A `notice` field means the result is incomplete and says why (empty index, scan in progress).
- The git tools require `basemind serve` to be running inside a git repository.
- `code_search` needs the `code-search` Cargo feature, `docs_search` needs `documents`, and the
  memory tools need `memory` (all in `--features full` and in the release binary). Without the
  feature the tool answers with an error naming it.
- Memory is scoped by the normalised `origin` remote URL (`git@github.com:Foo/bar.git` and
  `https://github.com/Foo/bar/` collapse to the same scope key): clones of the same repo share
  memory; unrelated repos do not see each other's entries.
- `docs_search` searches this repo's scope by default. Pages an older version ingested from the web
  stay under scope `web:<host>` and are no longer refreshed; pass `"scope":"web:<host>"` to search
  one host's pages.
- MCP resources are available for hosts that attach them: `basemind://status`,
  `basemind://outline/{path}` and `basemind://memory/{key}`.
