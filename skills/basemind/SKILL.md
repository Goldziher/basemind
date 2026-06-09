---
name: basemind
description: >-
  Navigate large or unfamiliar codebases via the basemind MCP server — outlines,
  symbol search, reference/caller lookups, commit history, blame, and diffs without
  reading source files. Reach for it whenever the user asks "where is X defined",
  "what calls Y", "what changed recently in Z", or whenever you're about to grep
  or open many files to find structural information.
---

# basemind — code-map MCP server

basemind is a tree-sitter-backed code map plus git context, served over MCP. It
pre-indexes a repository into a Fjall inverted index so structural and historical
questions resolve in milliseconds — without you reading whole files.

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

## Tool routing (copy this into your mental model)

| Question | Tool |
|---|---|
| "Where is X defined?" | `search_symbols` (substring match, optional `kind` filter) |
| "What's the shape of file F?" | `outline` (add `l2: true` for calls + docs) |
| "What calls X?" (any name) | `find_references` |
| "What calls this specific definition?" | `find_callers` (path + name + optional kind) |
| "What imports module M?" | `dependents` |
| "What files are indexed?" | `list_files` (filter by `language` or `path_contains`) |
| "What changed recently?" | `recent_changes`, `commits_touching`, `find_commits_by_path` |
| "When did symbol X last change?" | `symbol_history` |
| "Who wrote this line / symbol?" | `blame_file`, `blame_symbol` |
| "Where's the churn?" | `hot_files` |
| "What's dirty in the working tree?" | `working_tree_status` |
| "What's HEAD / branch?" | `repo_info` |
| "Show diff between revs for file F" | `diff_file`, `diff_outline` |
| "What's indexed?" | `status` |
| "Semantic search over PDFs / Office docs in the repo?" | `search_documents` (requires `--features documents`) |
| "Recall something the agent stored earlier?" | `memory_get` exact, `memory_list` prefix, `memory_search` KNN |
| "Remember this for future sessions?" | `memory_put` (delete with `memory_delete`) |

## Setup (one-time per repo)

basemind needs an index at `.basemind/` before it can answer queries. From the repo root:

```sh
basemind scan
```

This walks the tree, parses with tree-sitter, and writes a content-addressed blob
store + Fjall inverted index under `.basemind/`. A few seconds for small repos,
~15 s for a ~40k-file TypeScript monorepo.

The MCP server is launched by the host (`basemind serve` — wired up in
`.claude-plugin/plugin.json` for you). You do not start it manually.

Re-run `basemind scan` after large changes, or run `basemind watch` to keep the index fresh on file save.

If a tool returns "no indexed files", that means `basemind scan` hasn't been run in this repo yet.

## Examples

### Locating a symbol

```text
search_symbols { needle: "MapCache" }
→ src/mcp/mod.rs:79:1 MapCache (struct)
  src/mcp/mod.rs:88:1 MapCache (impl)
```

Now you know exactly where to read.

### Following references

```text
find_references { name: "process_file" }
→ src/scanner.rs:142:9 process_file
  src/scanner.rs:201:13 process_file
  ...
```

No need to grep — the index already knows.

### Outline a file before reading

```text
outline { path: "src/mcp/tools.rs" }
→ 21 #[tool] outline (function)
   112 #[tool] search_symbols (function)
   ...
```

A 1000-line file becomes a 30-line table of contents.

## Notes

- All paths are repository-relative with forward-slash separators.
- Lists are capped (`limit`, default 100, max 1000). Index scanners use
  `scan_cap = limit * 8` to bound work on common names.
- Matching is substring on names — `find_references("bar")` matches `Foo::bar()`
  and `bar()` alike. There is no scope resolution; cross-check with `outline` if
  disambiguation matters.
- Git tools require `basemind serve` to be running inside a git repository. Outside a git repo they return a clear error.
- Intelligence tools (`search_documents`, `memory_*`) require basemind to be built with
  `--features documents,memory`. Without them the tools dispatch but return an MCP error.
  Memory is scoped by the normalised `origin` remote URL (`git@github.com:Foo/bar.git` and
  `https://github.com/Foo/bar/` collapse to the same scope key) — clones of the same repo
  share memory; unrelated repos do not see each other's entries.
