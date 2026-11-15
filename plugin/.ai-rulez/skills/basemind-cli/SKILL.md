---
type: Playbook
title: Basemind Cli
description: Navigate codebases and manage caches via the basemind CLI — outlines,
  symbol search, reference/caller lookups, git history, blame, and diffs. For headless
  scripting, CI, or when driving the CLI more efficiently than interactive MCP calls.
  Shares the same index as the MCP server.
x-ai-rulez:
  kind: skill
  id: basemind-cli
  metadata:
    name: basemind-cli
    license: MIT
    metadata:
      author: Goldziher
      repository: https://github.com/Goldziher/basemind
---

# basemind CLI — the scriptable interface

basemind has two equally-weighted surfaces: MCP (interactive tool calls) and CLI (scriptable commands).
Every MCP tool has a CLI command that runs the same body (`code_grep` is `basemind code grep`,
`git_log` is `basemind git log`, `index_status` is `basemind status`); the CLI adds shorthands and a
few administrative commands that have no MCP tool. They share the same machine-global cache (Linux `~/.local/share/basemind/`, macOS
`~/Library/Application Support/basemind/`; override `BASEMIND_DATA_HOME`) and are safe to run
alongside each other. Reach for the CLI
when you're scripting, batching queries, running in headless environments, or CI.

## Running the CLI

The plugin runs its own version-matched binary, so launch the CLI through it:
`${CLAUDE_PLUGIN_ROOT}/scripts/mcp-launch.sh <args>` (in other hosts, `scripts/mcp-launch.sh` in the
plugin directory). Bare `basemind` from `PATH` is the fallback when no plugin launcher exists; a
different version there shares the same cache. Examples below write `basemind` for brevity.

## Capabilities

- **Code map across 300+ languages** — tree-sitter outlines, symbol search, call sites, callers,
  implementations, importers. Code only: markdown, JSON, YAML, TOML, XML, CSV and
  similar files are documents (`basemind memory documents`), not in the code map.
- **Text, symbol and meaning search** — literal or regex search over code content, symbol lookup by name, search by meaning.
- **Git history** — log, blame, and structural diffs at symbol resolution, plus churn.
- **Document RAG** — PDFs, Office, HTML, email, images (OCR, when the build has `documents-ocr`), markdown and config/data files → semantic search.
- **Shared memory** — per-repo, scope-keyed key-value + semantic memory across sessions.
- **Cache management** — stats, garbage collection, selective and full clears.

## When to reach for it

- Running in headless environments or CI pipelines.
- Batching multiple queries without interactive delays.
- Integrating basemind into shell scripts or non-MCP tooling.
- Controlling tool routing explicitly (no agent routing decisions).
- Clearing or sweeping caches destructively (only the offline `basemind cache clear` accepts
  `views` / `all`, it needs `--yes` off a terminal, and `blobs` wipes the machine-global store; only
  `basemind cache gc` deletes orphaned blobs).

**basemind first, shell/grep/git fallback.** Prefer `basemind code` over reading files, over
`grep`/`rg`, and over naked `git`: use it for code parsing (outlines, usages, callers), git
history / blame / diffs (`basemind git`), and document extraction / RAG / keyword + entity (NER) /
summary (`basemind memory documents`). Drop to raw shell, grep, or git only when no basemind
command covers the question.

## Command routing (copy this into your mental model)

| Question | Command | Notes |
|---|---|---|
| "What's the shape of file F?" | `basemind code outline path/F` | Symbols with 1-based line spans, plus imports. |
| "Where is X defined?" | `basemind code symbols "X" [--kind K --fuzzy]` | Ranked exact, prefix, substring; `--fuzzy` adds fuzzy matches last. |
| "One symbol's body, or a line span?" | `basemind code read path --symbol name` / `--line N --end-line M` | Alias `expand`. One form, not both. |
| "Where does this text appear?" | `basemind code grep "pattern" [--regex --path P --language L --context]` | Literal unless `--regex`; `--path` is a substring. |
| "Find code by meaning?" | `basemind code search "query" [--lane hybrid\|semantic\|keyword]` | Alias `semantic`. Needs `code-search`. |
| "What calls X?" (any name) | `basemind code refs "X" [--match substring]` | Alias `references`. Exact identifier match by default, no scope resolution. |
| "What calls this specific definition?" | `basemind code callers path name` | Shorthand for `refs name --path path`. |
| "What implements / extends X?" | `basemind code implementations "X"` | Shorthand for `refs X --relation implementations`. |
| "What imports module M?" | `basemind code dependents "M"` | Shorthand for `refs M --relation importers`. |
| "Which definition does this reference bind to?" | `basemind code definition path line name [--column C]` | Shorthand for `refs name --relation definition`. |
| "Which file is named like X?" | `basemind code find "X" [--path-prefix P --language L]` | Fuzzy; code and documents. |
| "What files are indexed?" | `basemind code files [--path-prefix P --language L]` | `find` with no query. |
| "What changed recently?" | `basemind git log [--limit N]` | Shorthand `git recent`. |
| "Which commits touched a path?" | `basemind git log --path P` | Shorthand `git touching P`; a trailing `/` means a directory. |
| "Which commit mentions X?" | `basemind git log --query "X" [--author A]` | Shorthand `git search`. |
| "Show one commit?" | `basemind git log --rev REV` | Shorthand `git show REV`. |
| "When did symbol X last change?" | `basemind git log --path path --symbol name` | Shorthand `git symbol-history path name`. |
| "Who wrote this line / symbol?" | `basemind git blame path [--line N --end-line M \| --symbol name]` | Shorthand `git blame-symbol path name`. |
| "What's dirty in the working tree?" | `basemind git diff` | Shorthand `git status`. |
| "Diff a file between revs?" | `basemind git diff path [--from A --to B] [--structural]` | Shorthand `git diff-outline path` for the structural form. |
| "Where's the churn?" | `basemind git churn [--window N --top-k K]` | CLI only. |
| "Commits whose paths match a regex?" | `basemind git by-path "regex"` | CLI only. |
| "What's indexed? What's HEAD / branch?" | `basemind status` | File count, languages, cache dir, branch, HEAD. `admin repo` prints the same. |
| "Search markdown / config / PDFs?" | `basemind memory documents "query" [--mime-type --keywords --entity --scope]` | Needs `documents` feature. |
| "Recall something stored earlier?" | `basemind memory search "q"` / `get "key"` / `list [--prefix P --tag T]` | By meaning / one note / a listing. |
| "Remember this for future sessions?" | `basemind memory put "key" "value" [--tag T]` | Delete with `memory delete "key"` (CLI only). |
| "Cache size?" | `basemind cache stats` | On-disk size + orphan accounting. CLI only, like the other cache commands. |
| "What cache space is reclaimable?" | `basemind cache gc --dry-run` | Report orphaned blobs without deleting them. |
| "Reclaim orphaned blobs?" | `basemind cache gc` | Cross-workspace sweep (blobs under 6 h old are kept). |
| "Score retrieval quality?" | `basemind admin eval --tasks tasks.jsonl` | CLI-only; see `benchmarks/eval/README.md`. |
| "Clear caches?" | `basemind cache clear --component blobs --yes` | Destructive; `--yes` is required off a terminal. `views` / `all` need the offline CLI. |
| "Keep index fresh?" | `basemind watch` | Live re-index watcher; no MCP server (that's `serve`). |
| "Refresh the index after edits?" | `basemind scan` | Full or incremental scan. |
| "Refresh changed paths?" | `basemind rescan [path…]` | Re-index in the live server. |
| "Per-tool activity summary?" | `basemind admin telemetry [--window today\|1h\|24h\|all]` | CLI only. Histogram + estimated tokens saved. |
| "Is this install healthy?" | `basemind doctor` | Checks root, config, index, grammars, hook, daemon; exits 1 on failure. |
| "Block commits from staling the index?" | `basemind hook install` | Pre-commit `scan --staged`; see Notes. |
| "Shell completions / man page?" | `basemind completions <shell>` / `basemind man` | Print to stdout. |

## Output format

By default, all commands return **human-readable text**. For machine consumption, add the global `--json` flag:

```bash
basemind code symbols "parseQuery" --json
```

This prints the same JSON the MCP tool returns (`agents` included). Human output
never shortens source bodies or diffs; the timing footer is written to stderr. Page long lists with
`--limit` plus `--cursor <next_cursor>` and bound them with `--max-tokens`.

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
basemind code outline src/mcp/tools.rs
```

### Get all call sites of a function

```bash
basemind code refs "process_file"
```

### Find all callers of a specific definition

```bash
basemind code callers src/scanner.rs "process_file" --json
```

### Show recent commits with changed files

```bash
basemind git log --limit 5
```

### Blame a symbol to see when its body last changed

```bash
basemind git blame src/scanner.rs --symbol "process_file"
```

### Manage cache space

```bash
basemind cache stats
basemind cache gc --dry-run
basemind cache clear --component blobs --yes
```

## Exit codes

`0` ok, `1` error, `2` usage (bad flag, or a destructive command without `--yes`), `3` busy (another
process holds the workspace writer lock; retry), `4` daemon unreachable, `130` interrupted. Ctrl-C
stops a scan cleanly: work already committed is kept.

## Notes

- `basemind hook install` resolves the hooks dir through git (`core.hooksPath` and linked worktrees
  work), never overwrites a pre-commit hook it did not write unless `--force` (the old one is kept
  as `pre-commit.bak`), and the installed hook fails open: it can never block a commit.
- Memory, `refs`/`callers`, the grep prefilter and `rescan` are forwarded to a running daemon
  when there is one, so they work alongside a live server.
- All paths are repository-relative with forward-slash separators.
- The CLI opens the index read-only; safe to run alongside a live `basemind serve` process.
- Lists are capped (`--limit`: symbols/grep/refs default 100, max 1000; files/find 200, max 5000;
  git log 20, max 100; search and documents 10).
- Every line number is 1-based. `symbols` ranks the exact name, then prefix, then substring, then
  (with `--fuzzy`) fuzzy matches; `refs` matches the identifier exactly (`bar` matches `Foo::bar()`
  and `bar()`, not `bar_baz()`) unless `--match substring`; `callers` pins one definition and marks
  the hits it proves `resolved`, or reports why it cannot (no resolution for the language, or no
  resolution facts for the file). `callers`, `read --symbol`, `git blame --symbol` and
  `git log --symbol` take `--line` to pick one of several same-named definitions.
- `outline` on a markdown/config/data file reports that it is a document; use `memory documents`.
- An unknown flag is a usage error (exit 2). `--format toon`, `--l2`, `--path-contains`,
  `--no-context` and `--line-start` / `--line-end` no longer exist.
- Git commands require basemind to be running inside a git repository.
- `code search` needs the `code-search` Cargo feature and the `memory` commands need `documents` /
  `memory` (all in `--features full` and in the release binary).
- Memory is scoped by the normalized `origin` remote URL — clones of the same repo share memory;
  unrelated repos do not see each other's entries.
