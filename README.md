# basemind

**Give your AI coding agent a brain for your repo.**

basemind is a code-map MCP server: it indexes your codebase into a queryable map
so AI coding agents — Claude Code, Cursor, Continue, anything that speaks
[MCP](https://modelcontextprotocol.io) — get instant semantic answers about your
code. **Where is this defined? Who calls it? When did it change? What's churning?**

Sub-millisecond queries. 300+ languages out of the box. Local-only. Built in Rust.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![crates.io](https://img.shields.io/crates/v/basemind.svg)](https://crates.io/crates/basemind)
[![npm](https://img.shields.io/npm/v/basemind.svg)](https://www.npmjs.com/package/basemind)
[![PyPI](https://img.shields.io/pypi/v/basemind.svg)](https://pypi.org/project/basemind/)
[![CI](https://github.com/Goldziher/basemind/actions/workflows/ci.yml/badge.svg)](https://github.com/Goldziher/basemind/actions/workflows/ci.yml)

---

## Why your agent needs this

Today, agents read code by **grepping blind**. Ask Claude "who calls `parseQuery`?"
and it ripgreps the string — you get hits in docs, tests, comments, and 14 unrelated
files. The agent burns context filtering noise, then guesses.

LSPs are the semantic answer, but they're single-language, slow to start, and
useless across a polyglot monorepo.

**basemind is the missing layer.** One index, every language, semantic-quality answers
at grep speed — exposed to the agent over MCP as concrete tools (`find_callers`,
`find_references`, `outline`, `symbol_history`, `blame_symbol`, `hot_files`, …)
instead of "go grep again."

---

## 30-second setup

**Install** (pick one):

```bash
brew install Goldziher/tap/basemind     # macOS, Linux
npm install -g basemind                 # any Node 14+ platform
pip install basemind                    # any Python 3.8+ platform
cargo install basemind --locked         # build from source
```

Opt-in **intelligence build** (PDF/Office ingestion, semantic doc search, shared
agent memory backed by LanceDB):

```bash
cargo install basemind --locked --features full
```

`full` is the meta-feature that turns on both `documents` (PDF / Office / HTML
ingestion + OCR + layout) and `memory` (shared agent memory + vector search).
Pulls in [`kreuzberg`](https://github.com/kreuzberg-dev/kreuzberg) (Elastic-2.0;
document parsing + bundled ONNX embeddings) and `lancedb` (embedded vector
store). First scan after enabling downloads the embedding model into the
kreuzberg cache; subsequent scans are warm.

**Index your repo:**

```bash
cd /path/to/your/repo
basemind scan
```

**Wire it into Claude Code** — install as a plugin:

```text
/plugin marketplace add Goldziher/basemind
/plugin install basemind@basemind
```

This registers basemind as an MCP server plus a `basemind` skill that tells
the model when to reach for code-map tools (instead of grepping or reading
files one by one). Restart the session and the agent has all the tools listed
below.

**Codex** — install via Codex's plugin / marketplace UI from the same repo
(the `.codex-plugin/plugin.json` manifest is shipped alongside Claude's), then
add the MCP server entry to `~/.codex/config.toml`:

```toml
[mcp_servers.basemind]
command = "basemind"
args = ["serve"]
```

**Other MCP clients** (Cursor, Continue, Cline, …) — drop the standard
`mcpServers` entry into the client's MCP config:

```json
{
  "mcpServers": {
    "basemind": {
      "command": "basemind",
      "args": ["serve"]
    }
  }
}
```

---

## What your agent gets

### Code-map tools

| Tool | What the agent can finally do |
|---|---|
| `outline` | "Give me this file's structure" — symbols, line/col, signatures, imports. One call replaces five Reads. |
| `search_symbols` | "Find anything named `useAuth`" — substring match across every indexed symbol, kind-filterable. |
| `workspace_grep` | Regex search across indexed files — returns line + column + matched text. |
| `find_references` | "Where is `parseQuery` called?" — indexed call-site lookup. No regex noise. |
| `find_callers` | "Who calls `User.save()`?" — resolves the definition first, then scans. |
| `call_graph` | "Trace the call chain into / out of `process_file` 3 levels deep" — BFS DAG over the call index. |
| `find_implementations` | "What implements `Drawable`?" — Fjall-backed trait/interface/base-class lookup. |
| `dependents` | "What imports this module?" — reverse import lookup. |
| `list_files` | "What files are in `src/auth/`?" — indexed path + language filters. |
| `status` | "What languages does this repo use?" — file count + language breakdown. |
| `repo_info` | Branch, HEAD, workdir at a glance. |
| `rescan` | Re-index after the agent edits code — full or `paths: [...]` for changed files only. |

### Git-aware tools

| Tool | What the agent can finally do |
|---|---|
| `symbol_history` \* | "When did `validateToken` actually change?" — tree-sitter × git, comment/format-stable diffs. |
| `blame_file` \* / `blame_symbol` \* | "Who wrote this and why?" — line-range or symbol-scoped blame. |
| `hot_files` | "What's been churning?" — top-K most-changed files in the last N commits. |
| `recent_changes` \* | "What changed recently on this branch?" |
| `commits_touching` \* | "Show me every commit that touched `auth.rs`." |
| `find_commits_by_path` \* | "Pickaxe: every commit whose changed-files match this regex." |
| `diff_outline` | "What symbols differ between `main` and `HEAD`?" — structural diff. |
| `diff_file` | "Give me the unified diff for `auth.rs` across these revs." |
| `working_tree_status` | "What's staged / unstaged / untracked right now?" |

\* Accepts `cursor` for pagination. Commit-iterator cursors (`recent_changes`,
`commits_touching`, `find_commits_by_path`, `symbol_history`) are bound to the HEAD sha at
mint time and surface `cursor_invalidated: true` if HEAD moves between calls. Blame cursors
(`blame_file`, `blame_symbol`) encode the last-returned hunk's `start_line` and resume
immediately after it.

### Intelligence tools (opt-in: `--features full`)

| Tool | What the agent can finally do |
|---|---|
| `search_documents` | Semantic KNN over PDFs / HTML / emails. Per-query overrides; JSON or TOON output. |
| `memory_put` / `memory_get` / `memory_list` | Persist scoped notes — exact-key store and prefix / tag scans. |
| `memory_search` | Semantic recall across stored memory entries — KNN over the LanceDB memory table. |
| `memory_delete` | Drop an entry from both Fjall and LanceDB. |

`search_documents` returns ISO 639-3 language codes per document (via
`whatlang`); tune detection through
`documents.language.{auto_detect,min_confidence,detect_multiple}` in TOML, the
matching `--documents-language-*` flags, or `BASEMIND_DOCUMENTS_LANGUAGE_*` env
vars.

Cross-encoder reranking is available as a per-query opt-in:

```text
search_documents(query, reranker_enabled=true, reranker_preset="bge-reranker-base")
```

The first reranker call downloads the ONNX weights (~278 MB) and caches them under
`~/.cache/kreuzberg/rerankers/`. Enable permanently via
`[documents.reranker] enabled = true` in `.basemind/basemind.toml`. Each reranked
hit gains a `rerank_score` field (cross-encoder relevance in `[0, 1]`).

Memory is scoped by the repo's normalised `origin` URL so clones share entries.
A repo with no remote falls back to a workdir-keyed scope (configurable via
`[memory].scope_strategy` in `.basemind/basemind.toml`).

### Web ingestion (opt-in: `--features crawl` or `--features full`)

| Tool | What the agent can finally do |
|---|---|
| `web_scrape` | Fetch one URL, extract markdown, chunk + embed, write to the documents store. |
| `web_crawl` | Follow links from a seed up to `[crawl].max_depth`, index each page. |
| `web_map` | "What URLs exist on this site?" — sitemap + link discovery without fetching bodies. |

Crawled pages land in the same LanceDB `documents` table as on-disk docs; the
default scope tag is `web:<host>` so `search_documents { scope: "web:docs.rs" }`
retrieves them together. `robots.txt` is honoured by default — flip it off only
via `[crawl].respect_robots_txt = false` in `.basemind/basemind.toml` (and only
for hosts you control). The crawler is HTTP-only; the browser, AI extraction,
and WARC archive features of the upstream
[`kreuzcrawl`](https://github.com/kreuzberg-dev/kreuzcrawl) engine are
deliberately not exposed.

Every tool returns JSON. Responses are capped (`limit`, default 100, max 1000) so
the agent's context doesn't explode.

### Pagination

Twelve tools support cursor-based pagination. When a response includes a
`next_cursor` field, pass it back as the `cursor` param on the next call to
fetch the next page. Callers that omit `cursor` see no behaviour change.

Three cursor backends, each with its own stability contract:

- **Fjall-backed** — `find_references`, `find_callers`, `find_implementations`,
  `memory_list`. Cursors remain valid across rescans because the underlying
  keys are content-addressed.
- **In-memory** — `search_symbols`, `list_files`. Cursors are invalidated if
  the cache rebuilds between calls. Responses carry `cursor_invalidated: true`
  in that case; the caller restarts pagination from the beginning.
- **Git-iterator** — `recent_changes`, `commits_touching`,
  `find_commits_by_path`, `symbol_history`. Cursors are bound to the HEAD sha
  at mint time. If HEAD moves between calls the response carries
  `cursor_invalidated: true` and the caller re-starts.
- **Deterministic blame** — `blame_file`, `blame_symbol`. Cursors encode the
  last-returned hunk's `start_line` and resume immediately after; no
  invalidation flag because the `(suspect_sha, path)` blame is deterministic.

---

## Visual integration: live stats in Claude Code

basemind writes one row per MCP tool call to `.basemind/telemetry.jsonl` (always on,
best-effort, ~200 bytes per row). Two surfaces consume it:

**Live statusline** — three lines in `~/.claude/settings.json`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "$HOME/.claude/plugins/basemind/.claude-plugin/statusline.sh",
    "refreshInterval": 5
  }
}
```

Renders `bm ~103f · scan 2m ago · 47 calls · ~14k tok saved` at the bottom of the
Claude Code terminal. Refreshes every 5 seconds. The script is shipped in the
plugin tree; Claude Code cannot auto-install statusline scripts so the wiring is
manual (one-time).

**On-demand dashboard** — the new `telemetry_summary` MCP tool returns the full
breakdown (per-tool histogram, per-baseline savings, last 10 calls). The
`/basemind-stats` skill renders it as markdown in the conversation.

The `est_tokens_saved` numbers are **heuristics** vs a disclosed grep+Read baseline.
Every row carries a `saved_baseline` label so the model is auditable. Tools without
a realistic baseline (`memory_*`, `search_documents`, git wrappers) record their
calls but report zero savings — we don't claim what we can't honestly measure.

---

## Performance

Measured by the in-repo hardening harness — Apple Silicon, release build,
`--features full`, default `eager_l2 = true`. Numbers are warm steady-state
(cold filesystem cache adds ~50% to scan time on the first run of a session).

### Scan throughput

| Repo | Files | Language mix | Cold scan |
|---|---|---|---|
| basemind (this repo) | 136 | Rust | < 1 s |
| tokio | 856 | Rust | 0.2 s |
| TypeScript compiler | 81 324 | TS / JS / JSON | 17–18 s |

The TypeScript clone is the worst case in the in-repo hardening harness. Most
real repos sit between these two extremes. Re-scans skip files whose content
hash is unchanged, so warm scans on edited working trees are typically
dominated by the changed-set size, not the repo size. The harness covers 8
upstream repos in total — see [§ Hardening](#hardening) for the full list.

### Per-tool MCP latency

Walk over the full code-map tool set against the TypeScript-compiler index
(81 324 files):

| Latency | Tools |
|---|---|
| < 1 ms | `outline`, `list_files`, `find_references`, `find_callers` |
| < 1 ms | `find_implementations`, `hot_files`, `repo_info` |
| 3–6 ms | `search_symbols`, `call_graph` |
| 4–10 ms | `recent_changes`, `commits_touching`, `find_commits_by_path` |
| 4–10 ms | `symbol_history`, `diff_outline`, `diff_file` |
| 20–25 ms | `status` (cross-file language breakdown) |
| 30–40 ms | `blame_file`, `blame_symbol` |
| 40–200 ms | `workspace_grep` (in-RAM regex over indexed files) |
| ~200 ms | `search_documents` (LanceDB KNN, opt-in) |
| 350–600 ms | `working_tree_status` (full `git status` walk) |

basemind preloads L1 outlines into RAM on `serve` start, so the code-map
queries are sub-millisecond — there's no per-call disk hit. The Fjall LSM
inverted index handles ref / caller / impl lookups without scanning blobs.
Git-tool latency tracks `gix` walk cost and dominates only on the largest
histories.

### What the query / parse consolidation bought

The L1 walk fuses symbols, imports, and implementations into one combined
tree-sitter query — one `QueryCursor`, one tree walk per file. The eager-L2
path then runs L2's calls + docs queries against the same parsed tree
instead of re-parsing. Two measurements against the consolidation:

| Repo | Before | After | Delta |
|---|---|---|---|
| tokio | 535 ms | **212 ms** | −60% |
| TypeScript compiler | 25.9 s | **17.5 s** | −32% |

---

## Languages

**300+ tree-sitter grammars** ship via
[tree-sitter-language-pack](https://github.com/kreuzberg-dev/tree-sitter-language-pack).
basemind dynamically loads them on first use and caches them locally.

Three tiers of coverage:

- **First-class** — hand-written `.scm` overrides in `src/queries/`. Full
  outlines (symbols, imports, calls, docs) **plus `find_implementations`**.
  Languages: Rust, Python, TypeScript, TSX, JavaScript.
- **First-class minus implementations** — full outlines, but
  `find_implementations` returns empty results by design. Go interface
  satisfaction is structural rather than syntactic, so there's nothing to
  capture. Languages: Go.
- **Best-effort** — TSLP `tags.scm` fallback, capture-renamed to basemind's
  shape by `adapt_tslp_tags`. Symbols and calls always work;
  `find_implementations` lights up for any TSLP grammar whose upstream
  `tags.scm` emits `@reference.implementation`. Covers ~100 grammars
  including Kotlin, C#, Swift, C++, Scala, Solidity, Lua, Ruby, PHP, Java.

Languages without an upstream `tags.scm` (JSON, YAML, TOML, plain `.properties`)
still parse and appear in `list_files`; they just don't expose symbols.

---

## Why basemind, specifically

- **Built for agents, not humans.** Every tool exists because an agent needs it,
  not because it makes a cute terminal demo.
- **Semantic quality, grep speed.** Tree-sitter parses → content-addressed blobs →
  Fjall LSM inverted index → sub-millisecond MCP responses.
- **Polyglot by default.** One index, every language. No LSP-per-language
  zoo. No "we don't support that yet."
- **Local-only.** No SaaS. No telemetry. No cloud round-trip. Your code never
  leaves the machine.
- **Deterministic.** Content-addressed blobs (blake3), stable hashes,
  reproducible across machines.
- **Pure Rust.** One static binary. No Python runtime, no Node runtime, no JVM.
  `basemind serve` adds < 80 MB to your agent's stack.

---

## CLI

basemind is also a CLI — useful for piping into shell tools, CI checks, or
just inspecting a repo without spinning up an MCP server.

```text
basemind init                              # write .basemind/basemind.toml with defaults
basemind scan                              # index the working tree
basemind scan --staged                     # index what's in git's staging area
basemind scan --rev <REV>                  # index a commit / branch / sha
basemind watch                             # long-running watcher; index on file change
basemind serve [--view <name>]             # MCP stdio server for agents
basemind query outline <path> [--l2]       # symbols, imports (+ docs/calls with --l2)
basemind query symbol <needle> [--kind K]  # substring search across symbols
basemind query dependents <module>         # reverse-lookup via imports
basemind hook install                      # install pre-commit hook (--staged scan)
basemind lang {list, install, clean}       # manage downloaded tree-sitter grammars
basemind cache clear                       # drop .basemind/git-cache/
```

Global flags: `-q/--quiet`, `-v/--verbose`, `--no-color` (NO_COLOR honored).

---

## Architecture

A short tour. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the long
version.

- **Scanner** (`src/scanner.rs`) — rayon-parallel walker over the gitignore-aware
  file set. Extracts L1 (symbols + imports + implementations), L2 (calls +
  docs), L3 (structural hashes) per file. One combined tree-sitter query per
  language drives the L1 walk; the parsed tree is shared between L1 and L2 on
  the eager-L2 path so each file is parsed exactly once.
- **Content-addressed blobs** (`src/store.rs`) — msgpack at
  `.basemind/blobs/<blake3>.{l1,l2,l3}.msgpack`. Two files with identical content
  share the same blob. Re-scan skips unchanged hashes.
- **Inverted index** (`src/index/`) — pure-Rust [Fjall](https://github.com/fjall-rs/fjall)
  LSM keyspace at `.basemind/views/<view>/index.fjall/`. Nine partitions
  (`symbols_by_path`, `symbols_by_name`, `calls_by_path`, `calls_by_callee`,
  `imports_by_module`, `imports_by_path`, `implementations_by_trait`,
  `implementations_by_path`, `embeddings`) drive symbol search, references,
  implementations, and dependents.
- **MCP surface** (`src/mcp/`) — stdio JSON-RPC via `rmcp`. Tool descriptions are
  the routing surface for agents; semantics (substring vs prefix, scope-aware vs
  name-only, capped) are stated honestly.
- **Git layer** (`src/git.rs`, `src/git_cache.rs`) — `gix`-backed blame, log,
  diff, status. Sha-keyed disk cache (`.basemind/git-cache/`) makes warm queries
  free.

### Views

A _view_ is a code map for a snapshot of the repo. Each view has its own index
under `.basemind/views/<view>/`; blobs are shared in `.basemind/blobs/`.

- **`working`** (default) — the on-disk working tree
- **`staged`** — git staging area; what's about to be committed
- **`rev-<sha7>`** — whatever you scanned with `basemind scan --rev <REV>`

They coexist — running one doesn't clobber the others. The pre-commit hook
installed by `basemind hook install` indexes `staged`, so the hook reflects
exactly what's being committed.

### Live refresh

Run `basemind watch` in one terminal and `basemind serve` in another: the server
watches the index, rebuilds its in-RAM map off-thread, and atomically swaps.
Queries reflect filesystem changes within ~150 ms with no `serve` restart.

---

## Hardening

basemind ships with a real-OSS hardening harness — 8 upstream repos (ripgrep,
tokio, microsoft/TypeScript, facebook/react, django, requests, gin, plus a
shallow ripgrep variant) cloned, scanned, and MCP-swept on every release. Canary
assertions catch regressions before they ship:

```sh
./scripts/harden.sh    # ~10 minutes; produces /tmp/basemind-harden/results.ndjson
```

The harness is `#[ignore]`-gated from normal `cargo test`. Invoked nightly and
on-dispatch from CI.

---

## Development

```sh
git clone https://github.com/Goldziher/basemind && cd basemind
task setup     # cargo fetch + prek install
task check     # lint + test
task build     # release binary
```

Pre-commit hooks via [prek](https://github.com/j178/prek) cover Rust
(`cargo fmt`/`clippy`/`sort`/`machete`/`deny`/`rustdoc-lint`), markdown, shell,
JSON/YAML/TOML, file-safety basics, and commit-message linting via
[gitfluff](https://github.com/Goldziher/gitfluff).

Contributing guidelines: see [`CONTRIBUTING.md`](CONTRIBUTING.md).

---

## Built on

basemind stands on three sibling crates from the
[kreuzberg-dev](https://github.com/kreuzberg-dev) family and the rest of the
Rust ecosystem:

- **[kreuzberg](https://github.com/kreuzberg-dev/kreuzberg)** — Elastic-2.0
  document parsing engine. Powers PDF / Office / HTML / email ingestion, OCR,
  layout detection, and the bundled ONNX embedding pipeline behind
  `search_documents`. Enabled via `--features documents` / `--features full`.
- **[kreuzcrawl](https://github.com/kreuzberg-dev/kreuzcrawl)** — HTTP-first
  web crawling engine. Powers `web_scrape`, `web_crawl`, and `web_map`.
  Enabled via `--features crawl` / `--features full`.
- **[tree-sitter-language-pack](https://github.com/kreuzberg-dev/tree-sitter-language-pack)**
  — the bundle of ~300 tree-sitter grammars + their `tags.scm` queries that
  drives every parser in basemind. The `adapt_tslp_tags` adapter in
  `src/lang.rs` rewrites upstream captures into basemind's shape so a single
  override surface covers all of them.

Plus [Fjall](https://github.com/fjall-rs/fjall) (pure-Rust LSM), `rmcp` (MCP
server / client), `gix` (pure-Rust git), `rayon` (data parallelism),
`tree-sitter`, and `lancedb`.

---

## License

[MIT](LICENSE).
