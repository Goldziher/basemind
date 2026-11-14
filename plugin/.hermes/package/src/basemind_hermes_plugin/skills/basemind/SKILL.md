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
Content-Hash: blake3:379562e8824457ef4e525f6f69d5e2839527d42368206996bff2c9cfb78c7a0a
Source-Hash: blake3:fbc7b4d75a7eb6eec0f28a10c0fe7172499c7b07b219d86838a7a4d3593da9ac
Schema-Version: v1
-->

# basemind — the indexed context layer

basemind is the full context layer for this repository, served over MCP. It pre-indexes the
repo into a content-addressed blob store + Fjall inverted index (and, when enabled, a LanceDB
vector store) so structural, historical, and semantic questions resolve in milliseconds —
without you reading whole files.

## Capabilities

- **Code map across 300+ languages** — tree-sitter outlines, symbol search, references,
  callers, call graphs, implementations, dependents. It holds **code only**: markdown, JSON,
  YAML, TOML, XML, CSV, INI and similar files are documents, searched with `memory` mode `documents`.
- **Full-text + symbol search** — `code` modes `grep` and `symbols`.
- **Git intelligence** — history, blame, and structural diffs at symbol resolution, plus churn.
- **Document RAG over 90+ file formats** — PDFs, Office, HTML, email, images (OCR), markdown and
  config/data files → semantic search with cross-encoder reranking (`memory` mode `documents`).
- **Shared memory** — per-repo, scope-keyed key-value + semantic memory across sessions.

## Dedicated per-capability skills

This umbrella skill covers the whole surface. For focused workflows, reach for the dedicated skills:

- **`basemind-code-search`** — outlines, symbol search, references, callers, call graphs.
- **`basemind-git-history`** — history, blame, structural diffs, churn.
- **`basemind-documents`** — document RAG (`memory` mode `documents`), memory.
- **`basemind-comms`** — coordinating with other agents in the same repo over the broker.
- **`basemind-cli`** — the same surface driven headlessly from the CLI.

## When to reach for it (instead of `grep` / `read_file`)

Use basemind for:

- **Locating a symbol**: "where is `Foo` defined?", "find the constructor for `Bar`", "show me every type ending in `Service`".
- **Following call graphs**: "what calls `process_file`?", "who depends on this module?".
- **Mapping a file's shape** before reading it: which symbols, in what order, with what signatures.
- **Walking recent history**: "what changed in this file in the last 20 commits?", "when did this symbol last change?".
- **Blame and ownership**: "who last touched this function?", "what commit introduced this line?".
- **Diffing across revisions**: "what symbols did this branch add?", "show the hunks for `foo.rs` between HEAD~5 and HEAD".

If you are about to open more than two or three files just to learn structure, stop
and use basemind first. The tools return paths + line numbers; you only `read_file`
once you know exactly which span you need.

## Context economy — the operating discipline

basemind tools return **paths, line numbers, and signatures — not file bodies**, so a
structural answer costs a fraction of the tokens of reading source. Treat that as the
default workflow, not an optimization:

- **Use `code` mode `outline` before you open a file.** Read the whole file only when you have already
  identified the exact span you need from the outline; then `read_file` that range, not the file.
- **Use `code` mode `symbols` instead of `grep`/`rg` for a definition.** It matches indexed symbol
  names and returns `path:line`, skipping the comment/string/test-name noise grep drowns you in.
- **Use `code` modes `references` / `callers` instead of grepping call sites.** Indexed call edges,
  not text matches.
- **Use `code` mode `grep` instead of shelling out to ripgrep** when you genuinely need regex over
  code content — it sweeps every indexed file (a trigram filter skips files that cannot match) and
  returns capped, structured hits with an exact `total_matches`.
- **Use `admin` mode `rescan` after you edit code**, not a server reconnect. Pass `paths: [...]` to limit it to
  the files you touched.
- MCP resources are available for hosts that attach them: `basemind://status`, `basemind://repo/map`,
  `basemind://outline/{path}` and `basemind://memory/{key}` (same data as `admin` `status`, `graph`
  `map`, `code` `outline`, `memory` `get`).
- **Do not re-read a file basemind already mapped.** If the outline answered the question, stop.

Rule of thumb: if a question is about _where_, _what calls_, _what shape_, _who changed_, or
_what's indexed_, a basemind tool answers it cheaper than reading files. Reach for `read_file`
only to see the actual implementation of a span you have already located.

**basemind first, shell/grep/git fallback.** Prefer basemind over reading files, over `grep`/`rg`,
and over naked `git`: use it for code parsing (outlines, references, callers), git history / blame /
diffs, and document extraction / RAG / keyword + entity (NER) / summary (`memory` mode `documents`).
Drop to raw shell, grep, or git only when no basemind tool covers the question.

## Tool routing (copy this into your mental model)

| Question | Tool |
|---|---|
| "Where is X defined?" | `code { mode: "symbols", name: X }` (substring; optional `kind`) |
| "Jump to the definition of X used here?" | `code { mode: "definition", path, line, column? }` |
| "What's the high-level architecture / module map?" | `graph { mode: "map" }` |
| "What's the shape of file F?" | `code { mode: "outline", path: F, l2?: true }` |
| "What calls X?" (any name) | `code { mode: "references", name: X }` |
| "What calls this specific definition?" | `code { mode: "callers", path: F, name: X }` |
| "Trace the call graph from a function?" | `graph { mode: "calls", name: X }` |
| "What implements / extends / inherits from X?" | `code { mode: "implementations", trait_name: X }` |
| "What imports module M?" | `code { mode: "dependents", module: M }` |
| "Which file is named like X?" | `code { mode: "find", query: X }` (fuzzy; not `find`/`ls -R`) |
| "Show me this one symbol's body" | `code { mode: "expand", path: F, name: X }` |
| "Find code by meaning, not spelling" | `code { mode: "semantic", query }` then `code { mode: "chunk", path, chunk_id }` |
| "What code files are indexed?" | `code { mode: "files" }` |
| "What changed recently?" | `git` modes `recent`, `touching`, or `by_path` |
| "When did symbol X last change?" | `git { mode: "symbol_history", path: F, name: X }` |
| "Who wrote this line / symbol?" | `git` mode `blame` or `blame_symbol` |
| "Where's the churn?" | `git { mode: "churn" }` |
| "What's dirty in the working tree?" | `git { mode: "status" }` |
| "What's HEAD / branch?" | `admin { mode: "repo" }` |
| "Show diff between revs for file F" | `git` mode `diff` or `diff_outline` |
| "What's indexed?" | `admin { mode: "status" }` |
| "Search PDFs / Office docs / markdown / config / data files?" | `memory { mode: "documents", query }` |
| "Recall something the agent stored earlier?" | `memory` mode `get`, `list`, or `search` |
| "Remember this for future sessions?" | `memory { mode: "put", key, value }` |
| "Refresh the index after editing code?" | `admin { mode: "rescan", paths?: […] }` |
| "Fetch next page of results?" | Pass `next_cursor` from prior response as `cursor` |
| "How much has basemind helped today?" | `admin { mode: "telemetry", window: "today" }` |

## Setup (one-time per repo)

basemind needs an index before it can answer queries. The index lives in a machine-global cache
(Linux `~/.local/share/basemind/`, macOS `~/Library/Application Support/basemind/`; override
`BASEMIND_DATA_HOME`), keyed by workspace — never inside your repo. From the repo root:

```sh
basemind scan
```

This walks the tree, parses with tree-sitter, and writes a content-addressed blob
store + Fjall inverted index into the machine-global cache. A few seconds for small repos,
~22 s for an ~80k-file TypeScript monorepo.

The MCP server is launched by the host (`basemind serve` — wired up in
`.claude-plugin/plugin.json` for you). You do not start it manually.

Re-run `basemind scan` after large changes, or run `basemind watch` to keep the index fresh on file save.

If a tool returns "no indexed files", that means `basemind scan` hasn't been run in this repo yet.

## Examples

### Locating a symbol

```text
code { mode: "symbols", name: "MapCache" }
→ src/mcp/mod.rs:79:1 MapCache (struct)
  src/mcp/mod.rs:88:1 MapCache (impl)
```

Now you know exactly where to read.

### Following references

```text
code { mode: "references", name: "process_file" }
→ src/scanner.rs:142:9 process_file
  src/scanner.rs:201:13 process_file
  ...
```

No need to grep — the index already knows.

### Outline a file before reading

```text
code { mode: "outline", path: "src/mcp/tools.rs" }
→ 21 code router (function)
   112 code helper (function)
   ...
```

A 1000-line file becomes a 30-line table of contents.

## Notes

- All paths are repository-relative with forward-slash separators.
- Lists are capped (`limit`: `symbols`/`grep`/`references`/`callers`/`implementations` default 100,
  max 1000; `files`/`find` 200, max 5000). Index scanners stop at `scan_cap = limit * 8` (min 2000)
  and set `total_is_partial`. `max_tokens` budgets a list; `format: "toon"` compacts it.
- `symbols` is a case-sensitive substring over names. `references` is a name-only substring: `name:
  "bar"` matches `Foo::bar()` and `bar()` alike, and it is complete (the fast floor). For one
  specific definition use `callers` (scope-resolved, marks proven hits `resolved`).
- Cursors from `references`/`callers`/`implementations` survive rescans; `symbols`/`grep`/`files`/
  `find` cursors do not (`cursor_invalidated`: restart the query).
- `outline`/`grep`/`symbols` do not see markdown or config/data files (`outline` on one errors
  "file not indexed"); `files` and `find` list documents too. Search their contents with `memory`
  mode `documents`, or Read the file.
- Git tools require `basemind serve` to be running inside a git repository. Outside a git repo they return a clear error.
- `memory` modes require basemind to be built with
  `--features full` (or the individual `documents` / `memory` flags). Without them the
  tools dispatch but return an MCP error.
  Memory is scoped by the normalised `origin` remote URL (`git@github.com:Foo/bar.git` and
  `https://github.com/Foo/bar/` collapse to the same scope key) — clones of the same repo
  share memory; unrelated repos do not see each other's entries.
- `memory` mode `documents` searches this repo's scope by default. Pages an older version ingested
  from the web stay in the `documents` LanceDB table under scope `web:<host>` and are no longer
  refreshed; pass `scope: "web:<host>"` to search one host's pages.
