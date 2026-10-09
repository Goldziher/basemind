---
name: basemind-cli
description: >-
  Navigate codebases and manage caches via the basemind CLI — outlines, symbol search,
  reference/caller lookups, git history, blame, and diffs. For headless scripting, CI, or when
  driving the CLI more efficiently than interactive MCP calls. Shares the same index as the
  MCP server.
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:2eecf8fb86c721b5d330766ab4be5a0e146bb8c32dea355125b3af3ca09fda0a
Source-Hash: blake3:2eee3eb4fe8e0cd3029634e4a8728976df0cff767bc90ab2516dadd0565fcd0d
Schema-Version: v1
-->

# basemind CLI — the scriptable interface

basemind has two equally-weighted surfaces: MCP (interactive tool calls) and CLI (scriptable commands).
They share the same machine-global cache (Linux `~/.local/share/basemind/`, macOS
`~/Library/Application Support/basemind/`; override `BASEMIND_DATA_HOME`) and are safe to run
alongside each other. Reach for the CLI
when you're scripting, batching queries, running in headless environments, or CI.

## Capabilities

- **Code map across 300+ languages** — tree-sitter outlines, symbol search, references, callers,
  call graphs, implementations, dependents. Code only: markdown, JSON, YAML, TOML, XML, CSV and
  similar files are documents (`basemind memory documents`), not in the code map.
- **Full-text + symbol search** — indexed regex over code content and substring symbol lookup.
- **Git intelligence** — history, blame, and structural diffs at symbol resolution, plus churn.
- **Document RAG over 90+ file formats** — PDFs, Office, HTML, email, images (OCR), markdown and config/data files → semantic search.
- **Shared memory** — per-repo, scope-keyed key-value + semantic memory across sessions.
- **Web crawl** — scrape / follow-link crawl into the searchable document store.
- **Cache management** — stats, garbage collection, selective and full clears.

## When to reach for it

- Running in headless environments or CI pipelines.
- Batching multiple queries without interactive delays.
- Integrating basemind into shell scripts or non-MCP tooling.
- Controlling tool routing explicitly (no agent routing decisions).
- Clearing or sweeping caches destructively (only the offline `basemind cache clear` accepts
  `views` / `all`, it needs `--yes` off a terminal, and `blobs` wipes the machine-global store; only
  `basemind cache gc` deletes orphaned blobs).

**basemind first, shell/grep/git fallback.** Prefer `basemind code` / `graph` over reading files, over
`grep`/`rg`, and over naked `git`: use it for code parsing (outlines, references, callers), git
history / blame / diffs (`basemind git`), document extraction / RAG / keyword + entity (NER) /
summary (`basemind memory documents`), and web scraping / crawling / sitemaps
(`basemind web scrape` / `crawl` / `map`). Drop to raw shell, grep, or git only when no basemind
command covers the question.

## Command routing (copy this into your mental model)

| Question | Command | Notes |
|---|---|---|
| "Where is X defined?" | `basemind code symbols "X"` | Substring match, optional `--kind` filter. |
| "What's the shape of file F?" | `basemind code outline path/F` | Add `--l2` for calls + docs. |
| "What calls X?" (any name) | `basemind code references "X"` | Name-only substring, no scope resolution; complete. |
| "What calls this specific definition?" | `basemind code callers path name [--kind]` | Specific definition lookup. |
| "Trace the call graph?" | `basemind graph calls "name" [--direction --max-depth]` | BFS over calls. |
| "What implements / extends X?" | `basemind code implementations "X"` | Rust, Python, TS/TSX, JS. |
| "What imports module M?" | `basemind code dependents "M"` | Reverse-lookup via imports. |
| "What code files are indexed?" | `basemind code files [--language --path-contains]` | Code only; filter by language/path. |
| "Which file is named like X?" | `basemind code find "X"` | Fuzzy filename search. |
| "One symbol's body?" | `basemind code expand path name` | Raw source of that symbol. |
| "What changed recently?" | `basemind git recent [--limit N]` | Recent commits with paths. |
| "When did symbol X last change?" | `basemind git symbol-history path name` | Cross-commit structural hash. |
| "Who wrote this line / symbol?" | `basemind git blame path` / `blame-symbol path name` | Per-line / per-symbol. |
| "Where's the churn?" | `basemind git churn [--window N --top-k K]` | Churn-ranked files. |
| "What's dirty in the working tree?" | `basemind git status` | Staged/unstaged summary. |
| "Diff a file between revs?" | `basemind git diff path old new` / `diff-outline path` | File / outline diffs. |
| "What's indexed?" | `basemind status` | File count, languages, cache dir. |
| "What's HEAD / branch?" | `basemind admin repo` | Branch, HEAD, origin. |
| "Regex over code contents?" | `basemind code grep "pattern" [--language --path-contains]` | Indexed code files; exact total. |
| "Search markdown / config / PDFs?" | `basemind memory documents "query"` | Needs `documents` feature. |
| "Recall something stored earlier?" | `basemind memory get "key"` / `list` / `search "q"` | KNN + exact match. |
| "Remember this for future sessions?" | `basemind memory put "key" "value"` | Delete with `memory delete "key"`. |
| "Cache size?" | `basemind cache stats` | On-disk size + orphan accounting. |
| "What cache space is reclaimable?" | `basemind cache gc --dry-run` | Report orphaned blobs without deleting them. |
| "Reclaim orphaned blobs?" | `basemind cache gc` | Cross-workspace sweep (blobs under 6 h old are kept). |
| "Score retrieval quality?" | `basemind admin eval --tasks tasks.jsonl` | CLI-only; see `benchmarks/eval/README.md`. |
| "Clear caches?" | `basemind cache clear --component blobs` | Destructive; `views` / `all` require the offline `basemind cache clear`. |
| "Pull this URL into RAG?" | `basemind web scrape <url>` | Single page (requires `--features crawl`). |
| "Ingest a docs site?" | `basemind web crawl <seed-url>` | Link-following crawl. |
| "What URLs exist on this site?" | `basemind web map <url>` | Sitemap + link discovery. |
| "Keep index fresh?" | `basemind watch` | Live re-index watcher; no MCP server (that's `serve`). |
| "Refresh the index after edits?" | `basemind scan` | Full or incremental scan. |
| "Refresh changed paths?" | `basemind rescan [path…]` | Re-index in the live server. |
| "Per-operation activity summary?" | `basemind admin telemetry` | Histogram + estimated tokens saved. |

## Output format

By default, all commands return **human-readable text**. For machine consumption, add the global `--json` flag:

```bash
basemind code symbols "parseQuery" --json
```

This returns the raw `JsonSchema`-derived response structure, same as MCP (`agents` and `workspace`
included). Human output never shortens source bodies, diffs or exports; the timing footer is written
to stderr. Page long lists with `--limit` plus `--cursor <next_cursor>`, bound them with
`--max-tokens`, and add `--format toon` for compact tables.

## Setup (one-time per repo)

```sh
basemind scan
```

This walks the tree, parses with tree-sitter, and writes the content-addressed blob store +
Fjall inverted index into the machine-global cache (Linux `~/.local/share/basemind/`, macOS
`~/Library/Application Support/basemind/`; override `BASEMIND_DATA_HOME`). A few seconds for small
repos, ~22 s for an ~80k-file TypeScript monorepo.

Re-run `basemind scan` after large changes, or run `basemind watch` to keep the index fresh.

## Examples

### Find where a symbol is defined

```bash
basemind code symbols "MapCache"
```

Output:

```text
src/mcp/mod.rs:79:1 MapCache (struct)
src/mcp/mod.rs:88:1 MapCache (impl)
```

### Show a file's outline before opening it

```bash
basemind code outline src/mcp/tools.rs --l2
```

### Get all references to a function

```bash
basemind code references "process_file"
```

### Find all callers of a specific definition

```bash
basemind code callers src/scanner.rs "process_file" --json
```

### Show recent commits with changed files

```bash
basemind git recent --limit 5
```

### Blame a symbol to see when its body last changed

```bash
basemind git blame-symbol src/scanner.rs "process_file"
```

### Manage cache space

```bash
basemind cache stats
basemind cache gc --dry-run
basemind cache clear --component blobs
```

## Notes

- All paths are repository-relative with forward-slash separators.
- The CLI opens the index read-only; safe to run alongside a live `basemind serve` process.
- Lists are capped (`--limit`: symbols/grep/references/callers/implementations default 100, max
  1000; files/find 200, max 5000).
- `symbols` is a case-sensitive substring; `references` is a name-only substring (`bar` matches
  `Foo::bar()` and `bar()`) and complete; `callers` is scope-resolved for one definition.
- `outline` on a markdown/config/data file errors "file not indexed"; use `memory documents`.
- Git tools require basemind to be running inside a git repository.
- `memory` modes require basemind to be built with `--features full`
  (or the individual `documents` / `memory` flags).
- Memory is scoped by the normalized `origin` remote URL — clones of the same repo share memory;
  unrelated repos do not see each other's entries.
- `web` modes `scrape`, `crawl`, and `map` require `--features crawl`.
