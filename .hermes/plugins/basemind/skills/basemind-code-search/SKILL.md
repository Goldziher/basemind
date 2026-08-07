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
Content-Hash: blake3:280fd8db9f3b191595c33108e858cbeb3697cc049476dcf34fd11dd8f54f4d45
Source-Hash: blake3:790db1582bcc08e9525d458dbfdd394dcb874071a62cc78df0d39a2dcd669ee7
Schema-Version: v1
-->

# basemind-code-search — navigate code without reading it

basemind pre-indexes the repo into a tree-sitter code map across 300+ languages. Structural
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
  content — it runs over the in-RAM index and returns capped, structured hits.
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
| "What files are indexed?" | `code { mode: "files" }` (filter by language/path) | `basemind code files [--language --path-contains]` |
| "Regex over file contents?" | `code { mode: "grep", pattern: "…" }` | `basemind code grep "pattern" [--language --path-contains]` |
| "What's indexed?" | `admin { mode: "status" }` | `basemind admin status` |
| "Refresh the index after editing?" | `admin { mode: "rescan", paths: […] }` | `basemind admin rescan [path…]` |
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

- Matching on symbol names is **substring**: `code` mode `references` with `name: "bar"` matches
  `Foo::bar()` and `bar()` alike. There is no scope resolution — cross-check with `code` mode
  `outline` when disambiguation matters.
- Lists are capped (`limit`, default 100, max 1000). Index scanners use `scan_cap = limit * 8` to
  bound work on common names.
- Needs an index in the machine-global cache (Linux `~/.local/share/basemind/`, macOS
  `~/Library/Application Support/basemind/`; override `BASEMIND_DATA_HOME`) — run `basemind scan`
  first (see the `basemind-scan` skill). "No indexed files" means the scan hasn't run in this repo yet.

For git history / blame / diffs see `basemind-git-history`; for document RAG and semantic search see
`basemind-documents`; for agent coordination see `basemind-comms`.
