# basemind

Code-map MCP server + scanner using tree-sitter. Maintains a queryable map of a repository
in `.basemind/`, refreshed on file changes. The single `basemind` binary is both a CLI
(`basemind scan`, `lang …`) and an MCP stdio server (`basemind serve`) for AI agents.

## Install

```bash
brew install Goldziher/tap/basemind   # macOS, Linux
npm install -g basemind               # any Node 14+ platform
pip install basemind                  # any Python 3.8+ platform
cargo install basemind --locked       # build from source
```

Pre-compiled binaries for `{x86_64,aarch64}-{linux-gnu,apple-darwin}` and
`x86_64-pc-windows-gnu` ship on [GitHub Releases](https://github.com/Goldziher/basemind/releases).
The `npm` and `pip` packages download the right binary on install / first run.

## Subcommands

```text
basemind init                              # write .basemind/basemind.toml with defaults
basemind scan                              # working-tree scan (default)
basemind scan --staged                     # index what's in the git staging area
basemind scan --rev <REV>                  # index a commit / branch / sha
basemind watch                             # long-running working-tree watcher
basemind serve [--view <name>]             # MCP server (stdio) for AI agents
basemind query outline <path> [--l2]       # symbols, imports (+ docs/calls with --l2)
basemind query symbol <needle> [--kind K]  # substring search across symbols
basemind query dependents <module>         # heuristic reverse-lookup
basemind hook install                      # install git pre-commit hook (uses --staged)
basemind lang {list, install, clean}       # manage downloaded tree-sitter grammars
basemind cache clear                       # drop .basemind/git-cache/
```

Global flags: `-q/--quiet`, `-v/--verbose`, `--no-color`
(NO_COLOR is also honored).

## Git views

A "view" is a code map for a specific snapshot of the repo. Each view has its own
index file under `.basemind/views/<view>/`; blobs are shared in `.basemind/blobs/`.

- **`working`** (default) — the on-disk working tree
- **`staged`** — the git staging area; what's about to be committed
- **`rev-<sha7>`** — whatever you scanned with `basemind scan --rev <REV>`

`basemind scan` (no flags) builds the `working` view. `basemind scan --staged` builds
`staged`. `basemind scan --rev HEAD~5` resolves to a 7-char sha and builds
`rev-<sha7>`. They coexist — running one doesn't clobber the others.

The pre-commit hook installed by `basemind hook install` runs `basemind scan --staged
--quiet`, so the hook indexes what's actually being committed rather than whatever
half-finished work is sitting in the working tree.

## MCP server

`basemind serve [--view <name>]` exposes the code map and git context to AI agents
over the canonical MCP
[stdio transport](https://modelcontextprotocol.io/specification/2025-11-25).
`--view` picks which scan to serve (default: `working`). All tools return JSON.

### Code-map tools

| Tool             | Use                                                            |
|------------------|----------------------------------------------------------------|
| `outline`        | full per-file structure: symbols + line/col + signatures + imports (`l2: true` for calls + docs) |
| `search_symbols` | substring lookup across every indexed file, with optional kind filter |
| `find_references`| call sites of any callee whose name matches; index-backed, no scope resolution |
| `find_callers`   | callers of a specific definition (path + name + kind); resolves def then scans |
| `list_files`     | enumerate indexed paths, optional `path_contains` + `language` filters |
| `dependents`     | heuristic reverse-lookup via imports                            |
| `status`         | repo overview: file count, language breakdown, cache directory  |

### Git tools (require `basemind serve` inside a git repo)

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

Sha-keyed git artifacts persist under `.basemind/git-cache/`. The cache has two tiers:
an in-process LRU (1024 entries per category by default, tune via
`basemind serve --git-cache-mem`) and a sha-keyed disk store
(`commit_files/<sha>.msgpack`, `log/<head_sha>__<scope>.msgpack`,
`blame/<sha>__<path_hash>.msgpack`).

Commits are immutable, so once an entry is on disk it's valid forever — the next
`basemind serve` reads it back without touching git. HEAD-keyed entries like `log`
naturally roll off when HEAD moves (the new sha defines a new key).

Drop the disk cache with `basemind cache clear`. Disable persistence per-run with
`basemind serve --no-git-cache-disk`.

### Live refresh

The MCP server watches its view's `index.msgpack`. When `basemind watch` rewrites
the index in another terminal, the server rebuilds its in-RAM code map off-thread
and atomically swaps. `search_symbols` and `dependents` reflect the new index
within ~150 ms; no `serve` restart needed.

The server opens the store **read-only** so it coexists with `basemind watch`.
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
    "basemind": {
      "command": "basemind",
      "args": ["serve"],
      "cwd": "/abs/path/to/your/repo"
    }
  }
}
```

## Languages

Any of the 300+ grammars shipped by
[tree-sitter-language-pack](https://github.com/kreuzberg-dev/tree-sitter-language-pack)
is eligible — grammars are dynamically downloaded on first use and cached at
`~/Library/Caches/tree-sitter-language-pack/`.

Hand-written extraction queries ship for **Rust, Python, TypeScript, TSX,
JavaScript, Go**: these get full outlines (signatures, kinds, decorators), call
sites, imports, and doc comments. Any other language for which TSLP ships a
vendored `tags.scm` (kotlin, csharp, swift, cpp, scala, solidity, lua, …
~100 grammars in the published bundle) gets best-effort symbol + call
extraction via the fallback adapter in `lang::adapt_tslp_tags`, which rewrites
the GitHub-standard `@definition.*` / `@reference.call` captures into basemind's
`@symbol.*` / `@call.*` shape. Languages with neither an override nor an
upstream `tags.scm` (JSON, YAML, TOML, …) still parse and land in `list_files`;
symbol/call extraction yields empty vectors for them.

Modern-JS patterns covered: arrow-function `const` declarations
(`const Foo = () => …`) and function-expression consts surface as kind `function`
rather than `const`, so a `search_symbols("Foo")` finds them. TSX has its own
query file (`src/queries/tsx.scm`); the dedupe pass in `extract/l1.rs` resolves
overlapping query matches by keeping the most specific kind. Rust `impl` blocks
surface as kind `impl` (the captured name is the implementing type).

TypeScript adds: `namespace Foo {…}` and ambient `module "foo" {…}` surface as
kind `namespace`; class accessors `get x()` and `set x(v)` surface as kinds
`getter` / `setter` (detected from the source bytes — promoting at extract time
from the generic `method` capture, since matching the `kind` keyword in a
tree-sitter predicate proved fragile across grammar versions).

Multi-line generic signature strings round-trip: the extracted signature walks
forward to the first `{` or `;` and collapses internal whitespace, so a
declaration like `function foo<\n  T extends Bar,\n  U extends Baz,\n>(x: T): U`
becomes `function foo< T extends Bar, U extends Baz, >(x: T): U` instead of being
truncated at the first newline.

Python decorator metadata travels with the decorated symbol: `@dataclass`,
`@property`, `@total_ordering`, etc. land on `Symbol.decorators` (empty `Vec`
when absent; serde skips serialization to keep responses tidy).

Known gaps (intentional, queued for follow-up): TS getter/setter discrimination
via tree-sitter query predicates (instead of the byte-level pre-check we ship),
generic type parameters on classes/interfaces, advanced `infer`/conditional-type
captures.

### Robustness knobs

- **`BASEMIND_PARSE_TIMEOUT_MS`** (default `5000`) — tree-sitter parse timeout
  per file. Tunes the progress-callback abort in `lang::parse_timed`.
- **`BASEMIND_GRAMMAR_OFFLINE`** (default unset) — when set to any non-empty
  non-`0` value, `lang::ensure_grammars` skips network downloads and returns
  a typed error if anything is missing. Pre-warm the cache first.
- **`BASEMIND_BLAME_MAX_BYTES`** (default `1048576`, 1 MiB) — per-file blame
  size cap. Larger files return `GitError::BlameTooLarge`, which the MCP
  layer surfaces as a `truncated_reason: "too_large"` response.
- **`BASEMIND_BLAME_MAX_LINES`** (default `5000`) — per-file blame line cap;
  guards against generated single-line monsters that pass the byte cap.
- **`BASEMIND_GIT_CACHE_LOG_MAX_BYTES`** (default `268435456`, 256 MiB) — one-
  shot LRU sweep budget for `.basemind/git-cache/log/` at server start.
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

The `symbol_history` tool ships with three fingerprint modes — pick via the
`hash_mode` request param, defaulting to `normalized`:

- **`normalized`** (default) — byte compare after `normalize_for_history` strips
  line + block comments per language and collapses whitespace runs to a single
  space. Cheap, language-aware, kills the dominant false-positive (autoformat /
  prettier / black / gofmt churn) at the cost of treating string-literal
  whitespace as non-significant.
- **`structural`** — AST-shape fingerprint built by walking the symbol's
  tree-sitter subtree and hashing `(node_kind, identifier_or_literal_text)`
  pairs. Comments and anonymous tokens contribute nothing — formatter-stable,
  comment-stable, _literal-sensitive_. A docstring rewrite still registers as a
  body change.
- **`structural_loose`** — same as `structural` but ignores literal _contents_
  (strings, numbers, booleans contribute only their node kind). Use when i18n
  string churn or numeric-constant tweaks dominate the noise.

All modes are accelerated by a `(blob_oid, lang) → FileMapL1` LRU cache on the
server: repeated visits to the same blob across commits skip the tree-sitter
parse entirely. The response echoes the mode that produced it.

### Submodules

`.gitmodules` is read at scan time and submodule roots are pre-filtered out of
the walk by default (`scan.skip_submodules = true`). The `status` tool surfaces
the list of detected submodules regardless of the knob, so clients can see the
boundary the scanner respects.

### Non-UTF-8 paths

Path fields use a `RelPath` (BString-backed) end to end. Paths with bytes that
aren't valid UTF-8 — common on Linux ext4 with deliberately exotic filenames,
rare elsewhere — survive scan → store → MCP without a lossy round-trip:

- **Wire format**: valid UTF-8 paths serialize as plain JSON / msgpack strings
  (no change for the typical case). Paths with invalid bytes fall back to
  `{"bytes": [u8...]}`. Deserialization accepts either shape plus raw msgpack
  `bin` blobs.
- **On-disk index**: `BTreeMap<RelPath, FileEntry>` — schema bumped to v4. v3
  caches auto-wipe on first read and re-scan.
- **Windows**: `OsStr::as_encoded_bytes()` yields WTF-8 (a UTF-8 superset that
  losslessly round-trips ill-formed UTF-16 such as unpaired surrogates).
  `RelPath` stores those bytes as-is; `Display` renders unpaired surrogates as
  `\u{NNNN}` escapes. Filesystem ops go through `OsStr::from_encoded_bytes`.

## Config

Lives at `.basemind/basemind.toml`. Shape is defined by
`schema/basemind-config-v1.schema.json` — the schema is the contract, Rust types
follow. Every TOML must declare its schema version:

```toml
"$schema" = "https://basemind.dev/schema/v1.json"
```

The loader validates the TOML against the JSON Schema (Draft 2020-12) before
deserializing into Rust types, so config errors surface with JSON Pointer paths
instead of "missing field" stack traces.

## Cache layers

- **blake3 file hash** — skip re-extract when content is unchanged.
- **Content-addressed msgpack blobs** at `.basemind/blobs/<hash>.l1.msgpack`
  (symbols + imports) and `.l2.msgpack` (docs + calls). Two source files with
  identical content share the same blob.
- **Schema bump auto-wipe** — when `SCHEMA_VER` increments, `Store::open`
  clears the cache automatically.

## Inverted index

`find_references` and `find_callers` are backed by a pure-Rust
[Fjall](https://github.com/fjall-rs/fjall) LSM key-value store at
`.basemind/views/<view>/index.fjall/`. The store is a _secondary_ index over the
canonical msgpack blobs — the L1/L2 maps still live in
`.basemind/blobs/<hash>.{l1,l2}.msgpack` as the source of truth.

Six Fjall keyspaces (plus a reserved `embeddings` partition for future vector
search):

| Keyspace            | Purpose                                                |
|---------------------|--------------------------------------------------------|
| `symbols_by_path`   | per-file outline lookups                               |
| `symbols_by_name`   | `name`-prefix range scans for symbol search            |
| `calls_by_path`     | per-file call lookups                                  |
| `calls_by_callee`   | `callee`-prefix range scans — drives `find_references` |
| `imports_by_module` | future fast-path for `dependents`                      |
| `embeddings`        | reserved for vector search; empty today                |

Key shapes (length-prefixed components — see `src/index/keys.rs`):

```text
symbols_by_path     u16:len(rel) ‖ rel ‖ start_byte:u32_be
symbols_by_name     u16:len(name) ‖ name ‖ kind:u8 ‖ u16:len(rel) ‖ rel ‖ start_byte:u32_be
calls_by_path       u16:len(rel) ‖ rel ‖ start_byte:u32_be
calls_by_callee     u16:len(callee) ‖ callee ‖ u16:len(rel) ‖ rel ‖ start_byte:u32_be
imports_by_module   u16:len(module) ‖ module ‖ u16:len(rel) ‖ rel ‖ start_byte:u32_be
embeddings          symbol_id:u64_be
```

Length-prefixed components guarantee prefix-scan isolation: a `Foo` prefix
never spills into `Foobar`. Schema version is stamped in the `meta` keyspace;
mismatch on open drops the whole `index.fjall/` directory and the next scan
rebuilds it.

### `eager_l2`

`find_references` only works when the index has been populated with L2 calls.
By default the scanner runs L2 extraction inline with L1
(`scan.eager_l2 = true`) — this roughly doubles scan time on large repos.
Flip it off if you don't need reference search and want the fastest scan
possible; `find_references` will return empty results until a foreground L2
pass is triggered.

### Vector search — deferred

The `embeddings` keyspace is reserved but unpopulated. Future iteration will
add an embedding hook (default candidates: `fastembed-rs` for local models, or
a pluggable HTTP endpoint) plus KNN lookup via [`usearch`](https://github.com/unum-cloud/usearch)
in-process (SIMD-accelerated HNSW, 2.25.x in 2026). The
`semantic_search` MCP tool ships with that work, not before.

## Hardening harness

`./scripts/harden.sh` clones a diverse set of upstream repos into
`/tmp/basemind-harden/` (ripgrep, tokio, microsoft/TypeScript, facebook/react,
django, requests, gin, plus a shallow ripgrep variant) and runs
`tests/harden.rs` against each. The harness drives every MCP tool over the
stdio transport via `rmcp`'s child-process client, records per-call latency to
`/tmp/basemind-harden/results.ndjson`, and asserts pass/fail criteria including:

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
# clones a handful of OSS repos into /tmp/basemind-bench/ and times cold/cached scans
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
