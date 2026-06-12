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
Pulls in `kreuzberg` (Elastic-2.0; document parsing + bundled ONNX embeddings)
and `lancedb` (embedded vector store). First scan after enabling downloads the
embedding model into the kreuzberg cache; subsequent scans are warm.

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
| `dependents` | "What imports this module?" — reverse import lookup. |
| `list_files` | "What files are in `src/auth/`?" — indexed path + language filters. |
| `status` | "What languages does this repo use?" — file count + language breakdown. |
| `repo_info` | Branch, HEAD, workdir at a glance. |

### Git-aware tools

| Tool | What the agent can finally do |
|---|---|
| `symbol_history` | "When did `validateToken` actually change?" — tree-sitter × git, comment/format-stable diffs. |
| `blame_file` / `blame_symbol` | "Who wrote this and why?" — line-range or symbol-scoped blame. |
| `hot_files` | "What's been churning?" — top-K most-changed files in the last N commits. |
| `recent_changes` | "What changed recently on this branch?" |
| `commits_touching` | "Show me every commit that touched `auth.rs`." |
| `diff_outline` | "What symbols differ between `main` and `HEAD`?" — structural diff. |
| `diff_file` | "Give me the unified diff for `auth.rs` across these revs." |
| `working_tree_status` | "What's staged / unstaged / untracked right now?" |

### Intelligence tools (opt-in: `--features full`)

| Tool | What the agent can finally do |
|---|---|
| `search_documents` | "Find the auth design doc" — semantic KNN over PDFs / Office / HTML / emails. |
| `memory_put` / `memory_get` / `memory_list` | Persist scoped notes — exact-key store and prefix / tag scans. |
| `memory_search` | Semantic recall across stored memory entries — KNN over the LanceDB memory table. |
| `memory_delete` | Drop an entry from both Fjall and LanceDB. |

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
and WARC archive features of the upstream `kreuzcrawl` engine are deliberately
not exposed.

Every tool returns JSON. Responses are capped (`limit`, default 100, max 1000) so
the agent's context doesn't explode.

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

A 39 270-file TypeScript repo. Apple Silicon, release build:

| What | Time |
|---|---|
| Cold scan (full index) | 12.4 s |
| Cached scan (no changes) | 1.6 s |
| MCP server startup | 3.1 s, 77 MB RSS |
| `status` query | 1.2 ms |
| `outline` (1571 symbols) | 1.9 ms |
| `search_symbols` | 1–3 ms |
| `find_references("spawn")` (tokio) | < 5 ms |

basemind preloads L1 outlines into RAM on `serve` start, so cross-file queries
are sub-millisecond. The Fjall LSM inverted index handles ref/caller lookups
without scanning blobs.

---

## Languages

**300+ tree-sitter grammars** ship via
[tree-sitter-language-pack](https://github.com/kreuzberg-dev/tree-sitter-language-pack).
basemind dynamically loads them on first use and caches them locally.

**First-class outlines** — full signatures, kinds, decorators, calls, imports,
docstrings — ship for:

> **Rust · Python · TypeScript · TSX · JavaScript · Go**

**Best-effort outlines** via the TSLP `tags.scm` fallback — covers ~100 grammars
including **Kotlin, C#, Swift, C++, Scala, Solidity, Lua, Ruby, PHP, Java**, …

Languages without an upstream `tags.scm` (JSON, YAML, TOML) still parse and
appear in `list_files`; they just don't expose symbols.

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
  file set. Extracts L1 (symbols + imports), L2 (calls + docs), L3 (structural
  hashes) per file.
- **Content-addressed blobs** (`src/store.rs`) — msgpack at
  `.basemind/blobs/<blake3>.{l1,l2,l3}.msgpack`. Two files with identical content
  share the same blob. Re-scan skips unchanged hashes.
- **Inverted index** (`src/index/`) — pure-Rust [Fjall](https://github.com/fjall-rs/fjall)
  LSM keyspace at `.basemind/views/<view>/index.fjall/`. Six keyspaces drive
  symbol search, reference lookup, dependents.
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

## License

[MIT](LICENSE).
