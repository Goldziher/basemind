---
name: basemind-code-search
description: >-
  Find where code is defined and used without reading files — symbol search, file outlines,
  references, callers, call graphs, implementations, dependents, and indexed regex over content.
  Reach for it whenever the user asks "where is X defined", "what calls Y", "what implements Z",
  "what's the shape of this file", or whenever you're about to grep or open files to learn structure.
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:94595d1f2a02b75bc9a4081daf389f5c3365bd720a2bac8270a74bae7c0a12b4
Source-Hash: blake3:d0ef8fea0654cc62042ee8040ad0fadbea594d97b0930187ff9f42359e443bd0
Schema-Version: v1
-->

# basemind-code-search — navigate code without reading it

basemind pre-indexes the repo into a tree-sitter code map across 300+ languages. The map holds
**code only**: markdown, JSON, YAML, TOML, XML, CSV, INI and `.env` files are documents, so none of
the modes below sees them (`outline` on one errors "file not indexed") — search them with `memory`
mode `documents` (see `basemind-documents`) or just Read them. Structural
questions — where a symbol lives, what calls it, what shape a file has — resolve from the index in
milliseconds and return **paths, line numbers, and signatures, not file bodies**. That is a fraction
of the tokens of reading source, so it is the default, not an optimization.

**basemind first, grep/read fallback.** If a question is about _where_, _what calls_, _what shape_,
or _what implements_, a basemind tool answers it cheaper than `grep`/`rg` or opening files. Drop to
raw shell only when no tool covers the question.

## The discipline

- **Use `code` mode `outline` before you open a file.** A 1000-line file becomes a 30-line table of contents.
  Read the actual source only once you have the exact span, then read _that range_, not the file.
- **Use `code` mode `symbols` instead of `grep` for a definition.** It matches indexed symbol names and
  returns `path:line`, skipping the comment/string/test-name noise grep drowns you in.
- **Use `code` modes `references` / `callers` instead of grepping call sites.** Indexed call edges, not
  text matches.
- **Use `code` mode `grep` instead of shelling out to ripgrep** when you genuinely need regex over
  code content — it sweeps every indexed file (a trigram filter skips files that cannot match) and
  returns capped, structured hits with an exact `total_matches`.
- **Do not re-read a file basemind already mapped.** If the outline answered the question, stop.
- **Use `admin` mode `rescan` after you edit code**, not a server reconnect. Pass `paths: [...]` to limit it.

## Tool routing

| Question | MCP tool | CLI |
|---|---|---|
| "Where is X defined?" | `code { mode: "symbols", name: "X" }` (substring, optional `kind`) | `basemind code symbols "X"` |
| "Jump to the definition of X used here?" | `code { mode: "definition", path: F, line }` (scope-aware) | `basemind code definition F line [--column]` |
| "What's the high-level architecture / module map?" | `graph { mode: "map" }` | `basemind graph map` |
| "What's the shape of file F?" | `code { mode: "outline", path: F }` (add `l2: true`) | `basemind code outline F [--l2]` |
| "What calls X?" (any name) | `code { mode: "references", name: "X" }` | `basemind code references "X"` |
| "What calls this specific definition?" | `code { mode: "callers", path: F, name }` | `basemind code callers F name [--kind]` |
| "Trace the call graph from a function?" | `graph { mode: "calls", name }` (bounded BFS) | `basemind graph calls "name" [--direction --max-depth]` |
| "What implements / extends / inherits X?" | `code { mode: "implementations", trait_name: "X" }` | `basemind code implementations "X"` |
| "What imports module M?" | `code { mode: "dependents", module: "M" }` | `basemind code dependents "M"` |
| "What code files are indexed?" | `code { mode: "files" }` (filter by language/path) | `basemind code files [--language --path-contains]` |
| "Which file is named like X?" | `code { mode: "find", query: "X" }` (fuzzy) | `basemind code find "X"` |
| "Show one symbol's body" | `code { mode: "expand", path: F, name }` | `basemind code expand F name` |
| "Find code by meaning?" | `code { mode: "semantic", query }`, then `code { mode: "chunk", path, chunk_id }` | `basemind code semantic "q"` |
| "Regex over file contents?" | `code { mode: "grep", pattern: "…" }` | `basemind code grep "pattern" [--language --path-contains]` |
| "What's indexed?" | `admin { mode: "status" }` | `basemind status` |
| "Refresh the index after editing?" | `admin { mode: "rescan", paths: […] }` | `basemind rescan [path…]` |
| "Fetch the next page?" | pass `next_cursor` from the prior response as `cursor` | — |

## Examples

```text
code { mode: "symbols", name: "MapCache" }
→ src/mcp/mod.rs:79:1 MapCache (struct)
  src/mcp/mod.rs:88:1 MapCache (impl)

code { mode: "references", name: "process_file" }
→ src/scanner.rs:142:9 process_file
  src/scanner.rs:201:13 process_file

code { mode: "outline", path: "src/mcp/tools.rs" }
→ 21 code router (function)
  112 code helper (function)
```

## Notes

- `symbols` is a case-sensitive **substring** over names. `references` is name-only: `name: "bar"`
  matches `Foo::bar()` and `bar()` alike, with no scope resolution, but it is the fast, complete
  floor. For one specific definition use `callers` (scope-resolved: hits it proves are marked
  `resolved`; trust `total` for completeness before a refactor). `dependents` is a substring over
  recorded imports and is unpaged.
- Lists are capped (`limit`: `symbols`/`grep`/`references`/`callers`/`implementations` default 100,
  max 1000; `files`/`find` 200, max 5000; `semantic` 10, max 100). Scanners stop at
  `scan_cap = limit * 8` (min 2000) and set `total_is_partial`; `grep` never caps files, so its
  `total_matches` is exact. `max_tokens` budgets a list, `format: "toon"` compacts it.
- Cursors from `references`/`callers`/`implementations` survive rescans; `symbols`/`grep`/`files`/
  `find` cursors do not (`cursor_invalidated`: restart the query).
- A `notice` (`warming_up`, `building_index`, `rescanning`) means results may be incomplete or a
  moment stale; a `projections_capped` notice appears only when no daemon was reachable.
- Needs an index in the machine-global cache (Linux `~/.local/share/basemind/`, macOS
  `~/Library/Application Support/basemind/`; override `BASEMIND_DATA_HOME`) — run `basemind scan`
  first (see the `basemind-scan` skill). "No indexed files" means the scan hasn't run in this repo yet.

For git history / blame / diffs see `basemind-git-history`; for document RAG and semantic search see
`basemind-documents`; for agent coordination see `basemind-comms`.
