---
description: Navigate codebases and manage caches via the basemind CLI — outlines, symbol search, reference/caller lookups, git history, blame, and diffs. For headless scripting, CI, or when driving the CLI more efficiently than interactive MCP calls. Shares the same index as the MCP server.
name: basemind-cli
license: MIT
metadata:
    author: Goldziher
    repository: https://github.com/Goldziher/basemind
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:439d48d0428b07e96e93af0a2b384652575fa82c960ff4f244cfa2c5aad95cc9
Source-Hash: blake3:eae55b5660df79ccb35563d12aabc8ef1ede337307fe5b9ed886c7aeb6b44a33
Schema-Version: v1
-->

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
  implementations, importers. The code map covers code only.
- **Text, symbol and meaning search** — literal or regex search (`code grep`) over code and
  markdown, config and data files, symbol lookup by name, search by meaning.
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
  `basemind cache gc` deletes orphaned blobs and the caches of removed worktrees).

**basemind first, shell/grep/git fallback.** Prefer `basemind code` over reading files, over
`grep`/`rg`, and over naked `git`: use it for code parsing (outlines, usages, callers), git
history / blame / diffs (`basemind git`), and document extraction / RAG / keyword + entity (NER) /
summary (`basemind memory documents`). Drop to raw shell, grep, or git only when no basemind
command covers the question.

## Command routing (copy this into your mental model)

| Question | Command | Notes |
|---|---|---|
| "What's the shape of file F?" | `basemind code outline path/F` | Symbols with 1-based line spans, plus imports. |
| "Where is X defined?" | `basemind code symbols "X" [--kind K --path P --language L --fuzzy]` | Ranked exact, prefix, substring; `--fuzzy` adds fuzzy matches last. |
| "One symbol's body, or a line span?" | `basemind code read path --symbol name` / `--line N --end-line M` | Alias `expand`. One form, not both. |
| "Where does this text appear?" | `basemind code grep "pattern" [--regex --path P --language L --context N]` | Literal unless `--regex`; `--path` is a substring. |
| "Find code by meaning?" | `basemind code search "query" [--lane hybrid\|semantic\|keyword]` | Alias `semantic`. Needs `code-search`. |
| "What calls X?" (any name) | `basemind code refs "X" [--match substring]` | Alias `references`. Exact identifier match by default, no scope resolution. |
| "What calls this specific definition?" | `basemind code callers path name` | Shorthand for `refs name --path path`. |
| "What implements / extends X?" | `basemind code implementations "X"` | Shorthand for `refs X --relation implementations`. |
| "What imports module M?" | `basemind code dependents "M"` | Shorthand for `refs M --relation importers`. |
| "Which definition does this reference bind to?" | `basemind code definition path line name [--column C]` | Shorthand for `refs name --relation definition`. |
| "Which file is named like X?" | `basemind code find "X" [--path P --language L]` | Fuzzy; code and documents. `P`: a prefix, or a glob with `*`. |
| "What files are indexed?" | `basemind code files [--path P --language L]` | `find` with no query. |
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
| "Search documents by meaning (PDFs, Office, HTML, email)?" | `basemind memory documents "query" [--format F --path GLOB --mime-type M --keywords K --entity E]` | Needs `documents` feature. |
| "Recall something stored earlier?" | `basemind memory search "q"` / `get "key"` / `list [--prefix P --tag T]` | By meaning / one note / a listing. |
| "Remember this for future sessions?" | `basemind memory put "key" "value" [--tag T]` | Delete with `memory delete "key"` (CLI only). |
| "Cache size?" | `basemind cache stats` | On-disk size + orphan accounting. CLI only, like the other cache commands. |
| "What cache space is reclaimable?" | `basemind cache gc --dry-run` | Report orphaned blobs without deleting them. |
| "Reclaim orphaned blobs and the caches of deleted worktrees?" | `basemind cache gc` | Reaps the caches of removed worktrees and groups no workspace names, then sweeps blobs across workspaces (blobs under 6 h old are kept). |
| "Score retrieval quality?" | `basemind admin eval --tasks tasks.jsonl` | CLI-only; see `benchmarks/eval/README.md`. |
| "Clear caches?" | `basemind cache clear --component blobs --yes` | Destructive; `--yes` is required off a terminal. `views` / `all` need the offline CLI. |
| "Keep index fresh?" | `basemind watch` | Live re-index watcher; no MCP server (that's `serve`). |
| "Build the index, or rebuild an empty, `degraded` or `rescan_needed` one?" | `basemind scan` | Full or incremental scan. |
| "Refresh files I edited?" | `basemind rescan <path>…` | Re-index just those paths in the live server, which already watches the tree. |
| "Per-tool activity summary?" | `basemind admin telemetry [--window today\|1h\|24h\|all]` | CLI only. Histogram + estimated tokens saved. |
| "Is this install healthy?" | `basemind doctor` | Checks root, config, index, grammars, hook, daemon; exits 1 on failure. |
| "Block commits from staling the index?" | `basemind hook install` | Pre-commit `scan --staged`; see Notes. |
| "Shell completions / man page?" | `basemind completions <shell>` / `basemind man` | Print to stdout. |

## Output format

By default, all commands return **human-readable text**. For machine consumption, add the global `--json` flag:

```bash
basemind code symbols "parseQuery" --json
```

This prints the same JSON the MCP tool returns (`agent` included). Human output
never shortens source bodies or diffs; the timing footer is written to stderr. Page long lists with
`--limit` plus `--cursor <next_cursor>` and bound them with `--max-tokens`.

## Lookups in another checkout

<!-- manual:cli -->
Every command takes `--root <dir>`, the checkout it answers about, so a script or an agent can
query a repository other than its working directory, and several repositories in one session,
without an MCP server. The query commands below answer from the index: none of them scans or
re-indexes the repository.

### The recipe

Name an indexed checkout by its absolute path in every command, ask for JSON and bound each call:

```sh
basemind status --root /work/checkout --json
basemind code symbols ExampleService --root /work/checkout --limit 10 --max-tokens 1200 --json
basemind code outline src/example_service.rs --root /work/checkout --json
basemind code callers src/example_service.rs handle --root /work/checkout --limit 20 --json
basemind git touching src/example_service.rs --root /work/checkout --limit 5 --json
```

Quote the path when it holds spaces or backslashes: a POSIX shell such as Git Bash reads an
unquoted backslash as an escape, and PowerShell takes `'C:\work\checkout'` as written.

### How the root resolves

- The directory named is widened to its workspace: the nearest directory at or above it holding a
  `basemind.toml`, `<dir>/.config/basemind.toml` or `<dir>/.config/basemind/config.toml`, up to the top of its
  git repository, else that top. A subdirectory with none of those between it and that top therefore
  queries the whole checkout's index, one under a nested config file queries that workspace's, and
  each worktree has its own.
  `basemind doctor --root /work/checkout` prints the root it resolved.
- A root must be a git repository or hold a `basemind.toml` (`basemind manual indexing`); a scan
  refuses any other directory, and so does a daemon a query is forwarded to.
- Path arguments are relative to the root, not to the shell's directory: `src/lib.rs` is
  `<root>/src/lib.rs` wherever the command runs. Write them with forward slashes.

### Reading the answer

1. **Exit status first.** A nonzero exit is no answer: do not parse its stdout. The error is on
   stderr; `2` is a usage error (an unknown flag, a path that is not indexed), `3` busy (retry),
   `4` an unreachable daemon (`basemind manual exit-codes`).
2. **Stdout is the answer.** With `--json` stdout holds only the response; read diagnostics from
   stderr, never mixed into it.
3. **Index state.** In `basemind status`, read the flags before the counts: `rebuild_in_progress`,
   `rescan_needed`, `partial`, `degraded` or a `notice` say the index is not whole, and while a
   rebuild holds the index `file_count` is 0 because the count is unavailable. With none of them
   set, a `file_count` of 0 means this root has no indexed files. Status describes the index, not
   each file: a query result lists a file edited since the scan in `stale`, and what that means
   depends on the command. `code outline` and `code read` answer from the file as it is now; a
   `code callers` or `code refs` hit keeps the line it had at the scan, so read the current source
   with `basemind code read` before using a stale call site. A missing, stale or busy index is a
   finding to report, not a reason to rebuild: building is a write. When one is allowed,
   `basemind scan --root /work/checkout` rebuilds the workspace (it takes no path) and
   `basemind rescan src/a.rs --root /work/checkout` re-indexes named files.
4. **Complete, partial or not ready.** A `notice` of `warming_up`, `building_index` or
   `rescanning` means retry shortly, and `empty_index` that the root has no index: an empty list
   beside a notice is not "no matches". `next_cursor` means more rows: repeat the call with
   `--cursor <next_cursor>` and the same arguments. `budgeted` means the token budget
   (`--max-tokens`, default 6000) trimmed the list. `truncated` with `truncated_reason` is a cut
   you did not ask for: with `scan_cap` the `total` is a lower bound, an empty git page cut at
   `window` is not the end, and `shallow_clone` makes an absent commit inconclusive
   (`basemind manual paging`).
5. **Candidates, not proof.** `basemind code callers` and `basemind code refs` list call sites
   matched by name, each graded by `resolution`. `resolved_total` counts the ones proven to bind to
   the pinned definition: `resolved_total: 0` beside candidates means none was proven, not that
   none exists, and a candidate may call another type's method of that name. `resolution_note`
   says why resolution could not run.
   `basemind code refs handle --path src/example_service.rs --resolution same_file,imported --root /work/checkout --json`
   keeps the proven rows only (`basemind manual resolution`). Confirm an answer that matters in
   source with `basemind code read src/example_service.rs --symbol handle --root /work/checkout`;
   an empty result does not prove absence.
6. **Lines are 1-based.** Every `line`, `end_line` and `column`, in arguments and responses, counts
   from 1, and a `code symbols` row reads `<path>:<line>: <signature>`: cite the number as it is.
<!-- /manual:cli -->

## Setup (one-time per repo)

```sh
basemind scan
```

This walks the tree, parses with tree-sitter, and writes the content-addressed blob store +
Fjall inverted index into the machine-global cache (Linux `~/.local/share/basemind/`, macOS
`~/Library/Application Support/basemind/`; override `BASEMIND_DATA_HOME`). A few seconds for small
repos, ~22 s for an ~80k-file TypeScript monorepo.

A running daemon (`basemind serve`) watches the tree and keeps the index fresh; without one, run
`basemind watch`, or `basemind rescan <path>…` for the files you changed.

## Examples

### Find where a symbol is defined

```bash
basemind code symbols "MapCache"
```

Output:

```text
results:
  src/engine/map_cache.rs:10: pub(crate) struct MapCache
  src/engine/map_cache.rs:39: impl MapCache
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

<!-- manual:exit-codes -->
| Code | Meaning |
|---|---|
| `0` | Success. |
| `1` | Runtime error. `basemind doctor` also exits 1 when a check fails. |
| `2` | Usage error: an unknown flag or argument, an invalid `rescan` path, or a destructive command without `--yes`. |
| `3` | Busy: another process holds the workspace writer lock, or the daemon a scan was forwarded to is stopping. Retry later. |
| `4` | A required daemon is unreachable. |
| `130` | Interrupted by Ctrl-C. Work already committed is kept and the index stays consistent. |

A failed command prints `Error:` and the cause chain on stderr. An MCP tool never exits: a failure
the caller can fix is a tool result with `isError` whose text names the fix.
<!-- /manual:exit-codes -->

`basemind manual` prints the full offline manual; `basemind manual <topic>` one page of it.

## Notes

- `basemind hook install` resolves the hooks dir through git (`core.hooksPath` and linked worktrees
  work), never overwrites a pre-commit hook it did not write unless `--force` (the old one is kept
  as `pre-commit.bak`, or `pre-commit.bak.1`, `.bak.2`, ... when earlier backups exist), and the
  installed hook fails open: it can never block a commit.
- Memory, `refs`/`callers`, the grep prefilter, `scan` and `rescan` are forwarded to a running daemon
  when there is one, so they work alongside a live server.
- All paths are repository-relative with forward-slash separators.
- The CLI opens the index read-only; safe to run alongside a live `basemind serve` process.
- Lists are capped (`--limit`: symbols/grep/refs default 100, max 1000; files/find 200, max 5000;
  git log 20, max 100; search and documents 10).
- Every line number is 1-based. `symbols` ranks the exact name, then prefix, then substring, then
  (with `--fuzzy`) fuzzy matches; `refs` matches the identifier exactly (`bar` matches `Foo::bar()`
  and `bar()`, not `bar_baz()`) unless `--match substring`; `callers` pins one definition, counts
  the hits it proves bind to it (`resolved_total`) and grades each one (`resolution`), or says why
  it cannot (`resolution_note`: no resolution for the language, or no resolution facts for the
  file). `callers`, `read --symbol`, `git blame --symbol` and
  `git log --symbol` take `--line` to pick one of several same-named definitions.
- `outline` on a markdown/config/data file reports that it is a document: find exact text in it
  with `code grep`, a topic with `memory documents`.
- An unknown flag is a usage error (exit 2); `basemind <command> --help` lists a command's flags.
- Git commands require basemind to be running inside a git repository.
- `code search` needs the `code-search` Cargo feature and the `memory` commands need `documents` /
  `memory` (all in `--features full` and in the release binary).
- Memory is one store per repository, keyed by the path of its main worktree: its worktrees
  share notes (per worktree when it has no `origin`); another clone, a moved repository or an
  `origin` that normalises differently does not see them, and a re-clone at the same path with
  the same `origin` does.
