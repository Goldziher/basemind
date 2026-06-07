# gitmind

File-watcher and code-map generator using tree-sitter. Maintains a queryable
map of a repository in `.gitmind/`, refreshed on file changes.

Prototype. See `/Users/naamanhirschfeld/.claude/plans/we-are-prototyping-a-frolicking-wozniak.md`
for the iteration plan.

## Subcommands

```text
gitmind init                              # write .gitmind/gitmind.toml with defaults
gitmind scan                              # working-tree scan (default)
gitmind scan --staged                     # index what's in the git staging area
gitmind scan --rev <REV>                  # index a commit / branch / sha
gitmind watch                             # long-running working-tree watcher
gitmind serve [--view <name>]             # MCP server (stdio) for AI agents
gitmind query outline <path> [--l2]       # symbols, imports (+ docs/calls with --l2)
gitmind query symbol <needle> [--kind K]  # substring search across symbols
gitmind query dependents <module>         # heuristic reverse-lookup
gitmind hook install                      # install git pre-commit hook (uses --staged)
gitmind lang {list, install, clean}       # manage downloaded tree-sitter grammars
gitmind cache clear                       # drop .gitmind/git-cache/
```

Global flags: `-q/--quiet`, `-v/--verbose`, `--no-color`
(NO_COLOR is also honored).

## Git views

A "view" is a code map for a specific snapshot of the repo. Each view has its own
index file under `.gitmind/views/<view>/`; blobs are shared in `.gitmind/blobs/`.

- **`working`** (default) — the on-disk working tree
- **`staged`** — the git staging area; what's about to be committed
- **`rev-<sha7>`** — whatever you scanned with `gitmind scan --rev <REV>`

`gitmind scan` (no flags) builds the `working` view. `gitmind scan --staged` builds
`staged`. `gitmind scan --rev HEAD~5` resolves to a 7-char sha and builds
`rev-<sha7>`. They coexist — running one doesn't clobber the others.

The pre-commit hook installed by `gitmind hook install` runs `gitmind scan --staged
--quiet`, so the hook indexes what's actually being committed rather than whatever
half-finished work is sitting in the working tree.

## MCP server

`gitmind serve [--view <name>]` exposes the code map and git context to AI agents
over the canonical MCP
[stdio transport](https://modelcontextprotocol.io/specification/2025-11-25).
`--view` picks which scan to serve (default: `working`). All tools return JSON.

### Code-map tools

| Tool             | Use                                                            |
|------------------|----------------------------------------------------------------|
| `outline`        | full per-file structure: symbols + line/col + signatures + imports (`l2: true` for calls + docs) |
| `search_symbols` | substring lookup across every indexed file, with optional kind filter |
| `list_files`     | enumerate indexed paths, optional `path_contains` + `language` filters |
| `dependents`     | heuristic reverse-lookup via imports                            |
| `status`         | repo overview: file count, language breakdown, cache directory  |

### Git tools (require `gitmind serve` inside a git repo)

| Tool                    | Use                                                                                  |
|-------------------------|--------------------------------------------------------------------------------------|
| `working_tree_status`   | porcelain shape: staged adds/mods/dels, modified, untracked, `is_clean` flag         |
| `recent_changes`        | last N commits on the current branch with per-commit file lists                      |
| `commits_touching`      | log filtered to a single path                                                        |
| `find_commits_by_path`  | regex over changed file paths in HEAD ancestry (cheap pickaxe)                       |
| `hot_files`             | top-K most-changed files in the last N commits (churn map)                           |
| `diff_outline`          | symbol-level diff: which symbols exist on each side of a rev                         |
| `diff_file`             | content-level unified-diff hunks for one file across two revs                        |
| `blame_file`            | per-hunk `(commit, author, time)` for a file, optionally clamped to a line range     |
| `blame_symbol`          | blame clamped to one tree-sitter symbol's lines (looked up via cached L1)            |
| `symbol_history`        | commits where a named symbol's body bytes changed — tree-sitter × git, the marquee   |
| `repo_info`             | workdir, branch name, HEAD sha                                                       |

### Git cache

Sha-keyed git artifacts persist under `.gitmind/git-cache/`. The cache has two tiers:
an in-process LRU (1024 entries per category by default, tune via
`gitmind serve --git-cache-mem`) and a sha-keyed disk store
(`commit_files/<sha>.msgpack`, `log/<head_sha>__<scope>.msgpack`,
`blame/<sha>__<path_hash>.msgpack`).

Commits are immutable, so once an entry is on disk it's valid forever — the next
`gitmind serve` reads it back without touching git. HEAD-keyed entries like `log`
naturally roll off when HEAD moves (the new sha defines a new key).

Drop the disk cache with `gitmind cache clear`. Disable persistence per-run with
`gitmind serve --no-git-cache-disk`.

### Live refresh

The MCP server watches its view's `index.msgpack`. When `gitmind watch` rewrites
the index in another terminal, the server rebuilds its in-RAM code map off-thread
and atomically swaps. `search_symbols` and `dependents` reflect the new index
within ~150 ms; no `serve` restart needed.

The server opens the store **read-only** so it coexists with `gitmind watch`.
On startup it preloads every L1 blob into RAM so cross-file queries are
sub-millisecond. Trade: startup time scales with file count.

Latency on a 39 270-file TypeScript repo:

| Tool             | Wall time |
|------------------|-----------|
| startup          | 3.1 s, 77 MB RSS |
| `status`         | 1.2 ms    |
| `list_files`     | 3 ms      |
| `outline` (1571 symbols) | 1.9 ms |
| `search_symbols` | 1–3 ms    |
| `dependents`     | 6.5 ms    |

Wire into Claude Code (`~/.claude.json`) or any MCP client:

```json
{
  "mcpServers": {
    "gitmind": {
      "command": "gitmind",
      "args": ["serve"],
      "cwd": "/abs/path/to/your/repo"
    }
  }
}
```

## Languages

Queries ship for **Rust, Python, TypeScript, TSX, JavaScript, Go**. Grammars are
dynamically downloaded via
[tree-sitter-language-pack](https://github.com/kreuzberg-dev/tree-sitter-language-pack)
(1.9.0-rc.22) on first use and cached at
`~/Library/Caches/tree-sitter-language-pack/`.

Modern-JS patterns covered: arrow-function `const` declarations
(`const Foo = () => …`) and function-expression consts surface as kind `function`
rather than `const`, so a `search_symbols("Foo")` finds them. TSX has its own
query file (`src/queries/tsx.scm`); the dedupe pass in `extract/l1.rs` resolves
overlapping query matches by keeping the most specific kind. Rust `impl` blocks
are surfaced as kind `impl` (the captured name is the implementing type).

Known gaps (intentional, queued for follow-up): TS namespaces, TS getters/setters,
generic-typed signature strings, Python decorator metadata on the decorated symbol.

### Robustness knobs

- **`GITMIND_PARSE_TIMEOUT_MS`** (default `5000`) — tree-sitter parse timeout
  per file. Tunes the progress-callback abort in `lang::parse_timed`.
- **`GITMIND_GRAMMAR_OFFLINE`** (default unset) — when set to any non-empty
  non-`0` value, `lang::ensure_grammars` skips network downloads and returns
  a typed error if anything is missing. Pre-warm the cache first.
- **`GITMIND_BLAME_MAX_BYTES`** (default `1048576`, 1 MiB) — per-file blame
  size cap. Larger files return `GitError::BlameTooLarge`, which the MCP
  layer surfaces as a `truncated_reason: "too_large"` response.
- **`GITMIND_BLAME_MAX_LINES`** (default `5000`) — per-file blame line cap;
  guards against generated single-line monsters that pass the byte cap.
- **`GITMIND_GIT_CACHE_LOG_MAX_BYTES`** (default `268435456`, 256 MiB) — one-
  shot LRU sweep budget for `.gitmind/git-cache/log/` at server start.
  `0` disables eviction.

### Shallow clones

`Repo::is_shallow()` is true when `.git/shallow` exists. History-walking MCP
tools (`recent_changes`, `commits_touching`, `blame_file`, `blame_symbol`,
`symbol_history`) add `"truncated": true, "truncated_reason": "shallow_clone"`
to their response. Blame additionally recovers from gix's "could not find
existing iterator over a tree" error at the shallow boundary by returning an
empty hunk list with the same truncated flag, instead of a hard MCP error.

### Binary files

Pre-flight NUL-byte scan in the first 8 KiB skips binaries that masquerade as
source via a `.ts`/`.py`/etc. extension — counted as `skipped_binary` rather
than `skipped_non_utf8`. See `scanner::looks_binary`.

### Merge commits

`commit_files` now unions diffs against every parent (not just the first), so
octopus merges no longer drop changes from non-first-parent legs. Per-path
status uses max severity (`Added > Modified ≈ Renamed > Deleted`) when the same
file shows up with different kinds across parents.

### `symbol_history` stability

`find_symbol_bytes` runs each symbol's bytes through
`mcp::helpers::normalize_for_history` before equality comparison: line + block
comments stripped per language, whitespace runs collapsed to one space. Kills
the dominant false-positive (autoformat / prettier / black / gofmt churn) at
the cost of treating string-literal whitespace as non-significant.

## Config

Lives at `.gitmind/gitmind.toml`. Shape is defined by
`schema/gitmind-config-v1.schema.json` — the schema is the contract, Rust types
follow. Every TOML must declare its schema version:

```toml
"$schema" = "https://gitmind.dev/schema/v1.json"
```

The loader validates the TOML against the JSON Schema (Draft 2020-12) before
deserializing into Rust types, so config errors surface with JSON Pointer paths
instead of "missing field" stack traces.

## Cache layers

- **blake3 file hash** — skip re-extract when content is unchanged.
- **Content-addressed msgpack blobs** at `.gitmind/blobs/<hash>.l1.msgpack`
  (symbols + imports) and `.l2.msgpack` (docs + calls). Two source files with
  identical content share the same blob.
- **Schema bump auto-wipe** — when `SCHEMA_VER` increments, `Store::open`
  clears the cache automatically.

## Hardening harness

`./scripts/harden.sh` clones a diverse set of upstream repos into
`/tmp/gitmind-harden/` (ripgrep, tokio, microsoft/TypeScript, facebook/react,
django, requests, gin, plus a shallow ripgrep variant) and runs
`tests/harden.rs` against each. The harness drives every MCP tool over the
stdio transport via `rmcp`'s child-process client, records per-call latency to
`/tmp/gitmind-harden/results.ndjson`, and asserts pass/fail criteria including:

- no tool exceeds the 90 s wall-clock ceiling,
- React canary: `search_symbols("useState")` returns ≥ 1 hit,
- shallow ripgrep canary: at least one history-walking tool surfaces
  `truncated: true`,
- no tool errors except documented "not found"-class outcomes.

It's the gating artifact for the hardening track — `#[ignore]`'d so it doesn't
run in normal `cargo test`. Invoked nightly + on-dispatch from CI
(`.github/workflows/ci.yml`'s `hardening` job).

## Bench

```sh
# clones a handful of OSS repos into /tmp/gitmind-bench/ and times cold/cached scans
./scripts/bench.sh
```

Reference numbers on Apple Silicon (release build):

| Repo       | Files | Cold scan | Cached scan |
|------------|-------|-----------|-------------|
| ripgrep    | 100   | 148 ms    | 25 ms       |
| tokio      | 779   | 160 ms    | 70 ms       |
| django     | 3030  | 720 ms    | 130 ms      |
| TypeScript | 39270 | 12.4 s    | 1.6 s       |

The TypeScript scan flags ~15k files with `had_errors: true` — those are the
compiler's intentionally-broken `tests/cases/*` fixtures. Partial-parse
extraction recovers what it can, marks the file, and keeps the well-formed
siblings queryable.

## Tests

```sh
cargo test           # 46 tests across 9 suites
cargo test --test harden -- --ignored   # gating real-OSS harness (see Hardening harness)
```

Suite breakdown: 13 lib unit tests + 6 config schema + 5 git_cache + 4 git_smoke

- 16 scan_smoke + 1 schema_bump + 1 mcp_smoke (end-to-end stdio MCP against a
tiny synthetic repo, runs in ~1.3 s). The MCP smoke is the cargo-test-time
counterpart to the heavier `harden.rs` gating harness.

## Development

Pre-commit hooks managed via [prek](https://github.com/j178/prek):

```sh
prek install         # one-time, installs pre-commit + commit-msg hooks
prek run --all-files # run on every tracked file
```

Hooks cover Rust (`cargo fmt`/`clippy`/`sort`/`machete`/`deny`/`rustdoc-lint`),
markdown, shell, JSON/YAML/TOML, file-safety basics, and commit-message linting
via [gitfluff](https://github.com/Goldziher/gitfluff). Config is in
`.pre-commit-config.yaml`.
