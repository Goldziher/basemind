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
Content-Hash: blake3:100d90f7c93231f2e990888958643782cd9384b2262eb3d43df1a04652004f74
Source-Hash: blake3:790db1582bcc08e9525d458dbfdd394dcb874071a62cc78df0d39a2dcd669ee7
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
  call graphs, implementations, dependents.
- **Full-text + symbol search** — indexed regex over content and substring symbol lookup.
- **Git intelligence** — history, blame, and structural diffs at symbol resolution, plus churn.
- **Document RAG over 90+ file formats** — PDFs, Office, HTML, email, images (OCR) → semantic search.
- **Shared memory** — per-repo, scope-keyed key-value + semantic memory across sessions.
- **Web crawl** — scrape / follow-link crawl into the searchable document store.
- **Cache management** — stats, garbage collection, selective and full clears.

## When to reach for it

- Running in headless environments or CI pipelines.
- Batching multiple queries without interactive delays.
- Integrating basemind into shell scripts or non-MCP tooling.
- Controlling tool routing explicitly (no agent routing decisions).
- Clearing caches destructively (only the CLI allows `--component all`).

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
| "What calls X?" (any name) | `basemind code references "X"` | Name match, no scope resolution. |
| "What calls this specific definition?" | `basemind code callers path name [--kind]` | Specific definition lookup. |
| "Trace the call graph?" | `basemind graph calls "name" [--direction --max-depth]` | BFS over calls. |
| "What implements / extends X?" | `basemind code implementations "X"` | Rust, Python, TS/TSX, JS. |
| "What imports module M?" | `basemind code dependents "M"` | Reverse-lookup via imports. |
| "What files are indexed?" | `basemind code files [--language --path-contains]` | Filter by language/path. |
| "What changed recently?" | `basemind git recent [--limit N]` | Recent commits with paths. |
| "When did symbol X last change?" | `basemind git symbol-history path name` | Cross-commit structural hash. |
| "Who wrote this line / symbol?" | `basemind git blame path` / `blame-symbol path name` | Per-line / per-symbol. |
| "Where's the churn?" | `basemind git churn [--window N --top-k K]` | Churn-ranked files. |
| "What's dirty in the working tree?" | `basemind git status` | Staged/unstaged summary. |
| "Diff a file between revs?" | `basemind git diff path old new` / `diff-outline path` | File / outline diffs. |
| "What's indexed?" | `basemind admin status` | File count, languages, cache dir. |
| "What's HEAD / branch?" | `basemind admin repo` | Branch, HEAD, origin. |
| "Regex over file contents?" | `basemind code grep "pattern" [--language --path-contains]` | Full-text search. |
| "Semantic search over docs?" | `basemind memory documents "query"` | Needs `documents` feature. |
| "Recall something stored earlier?" | `basemind memory get "key"` / `list` / `search "q"` | KNN + exact match. |
| "Remember this for future sessions?" | `basemind memory put "key" "value"` | Delete with `memory delete "key"`. |
| "Cache size?" | `basemind admin cache-stats` | On-disk size + orphan accounting. |
| "What cache space is reclaimable?" | `basemind admin gc` | Report orphaned blobs without deleting them. |
| "Clear caches?" | `basemind admin cache-clear --component blobs --confirm` | Destructive; `views` / `all` require the offline cache command. |
| "Pull this URL into RAG?" | `basemind web scrape <url>` | Single page (requires `--features crawl`). |
| "Ingest a docs site?" | `basemind web crawl <seed-url>` | Link-following crawl. |
| "What URLs exist on this site?" | `basemind web map <url>` | Sitemap + link discovery. |
| "Keep index fresh?" | `basemind watch` | Live re-index watcher; no MCP server (that's `serve`). |
| "Refresh the index after edits?" | `basemind scan` | Full or incremental scan. |
| "Refresh changed paths?" | `basemind admin rescan [path…]` | Re-index in the live server. |
| "Per-operation activity summary?" | `basemind admin telemetry` | Histogram + estimated tokens saved. |

## Output format

By default, all commands return **human-readable text**. For machine consumption, add the global `--json` flag:

```bash
basemind code symbols "parseQuery" --json
```

This returns the raw `JsonSchema`-derived response structure, same as MCP.

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
basemind admin cache-stats
basemind admin gc
basemind admin cache-clear --component blobs --confirm
```

## Notes

- All paths are repository-relative with forward-slash separators.
- The CLI opens the index read-only; safe to run alongside a live `basemind serve` process.
- Lists are capped (`--limit`, default 100, max 1000).
- Matching on symbol names is substring-based; `code` mode `references` with `name: "bar"` matches
  `Foo::bar()` and `bar()` alike.
- Git tools require basemind to be running inside a git repository.
- `memory` modes require basemind to be built with `--features full`
  (or the individual `documents` / `memory` flags).
- Memory is scoped by the normalized `origin` remote URL — clones of the same repo share memory;
  unrelated repos do not see each other's entries.
- `web` modes `scrape`, `crawl`, and `map` require `--features crawl`.
