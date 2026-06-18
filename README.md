<!-- markdownlint-disable MD033 MD041 -->
<div align="center">

# basemind

Full AI context layer for coding agents — code-map, document RAG, shared memory, web crawl,
git history. 300+ languages, one MCP server.

[![crates.io](https://img.shields.io/crates/v/basemind?style=flat-square)](https://crates.io/crates/basemind)
[![npm](https://img.shields.io/npm/v/basemind?style=flat-square)](https://www.npmjs.com/package/basemind)
[![PyPI](https://img.shields.io/pypi/v/basemind?style=flat-square)](https://pypi.org/project/basemind/)
[![CI](https://img.shields.io/github/actions/workflow/status/Goldziher/basemind/ci.yaml?style=flat-square)](https://github.com/Goldziher/basemind/actions/workflows/ci.yaml)
[![License: MIT](https://img.shields.io/badge/license-MIT-green?style=flat-square)](LICENSE)

[Pillars](#the-four-pillars) · [Tools](#feature-table) · [Quickstart](#quickstart) · [Performance](#performance) · [Architecture](#architecture) · [Install](#installation)

</div>

---

## Live statusline (Claude Code)

```text
◆ basemind  ●  1,247 files · 23m ago  │  47 calls · 14k saved  │  312 docs · 18 mem · 4 sites
```

Enable with `/bm-statusline` — see the [Statusline](#statusline) section for details.

---

## The four pillars

**Code** — Tree-sitter outlines, symbol search, reference + caller + implementation graphs,
call chains, git history per symbol, blame at symbol-level resolution.

**Documents** — Ingest + semantic search over PDFs, Office (Word/Excel/iWork), HTML, email,
archives. Built-in OCR, layout detection, keyword + NER extraction, cross-encoder reranking.
All ONNX bundled — no system install needed.

**Memory** — Per-repo scoped key-value + semantic vector storage. Clones of the same git
origin automatically share memory; unrelated repos isolated.

**Web** — On-demand HTTP scrape + follow-link crawl. Pages chunk, embed, and land in the
documents store under scope `web:<host>` for unified search.

---

## Feature table

<!-- markdownlint-disable MD013 -->

| Pillar | What it does | MCP tools | Backend |
|---|---|---|---|
| **Code intelligence** | Outlines, symbol search, refs/callers/callees, call graphs, impl lookup, dependents, in-tree regex | `outline`, `search_symbols`, `workspace_grep`, `find_references`, `find_callers`, `call_graph`, `find_implementations`, `dependents`, `list_files`, `status`, `repo_info` | tree-sitter × 300+ langs · Fjall LSM index · content-addressed blob store |
| **Git intelligence** | Symbol-level history, blame, churn, recent changes, structural diffs across revs | `symbol_history`, `blame_file`, `blame_symbol`, `hot_files`, `recent_changes`, `commits_touching`, `find_commits_by_path`, `diff_outline`, `diff_file`, `working_tree_status` | gix + sha-keyed disk cache |
| **Document RAG** | Ingest + semantic search over PDFs, Office (Excel/Word/HWP/iWork), HTML, XML, email, archives. Adds OCR (Tesseract + PaddleOCR), cross-encoder reranker, keyword extraction (YAKE/RAKE), NER (gline-rs ONNX + LLM), extractive + abstractive summarization, layout detection, page auto-rotate, redaction, language detection. All ONNX models bundled — no system install needed. | `search_documents` | kreuzberg + LanceDB |
| **Shared memory** | Per-repo scoped key-value + semantic memory. Clones of the same git origin URL automatically share memory; unrelated repos isolated. | `memory_put`, `memory_get`, `memory_list`, `memory_search`, `memory_delete` | LanceDB + Fjall, scope-keyed |
| **Web crawl** | On-demand HTTP scrape + link-following crawl. Crawled pages route through the documents pipeline (chunk → embed → LanceDB) under scope `web:<host>`. | `web_scrape`, `web_crawl`, `web_map` | kreuzcrawl (native HTTP, no chromium) |
| **Admin** | Live rescan + telemetry dashboard | `rescan`, `telemetry_summary` | — |

<!-- markdownlint-enable MD013 -->

---

## Quickstart

### Claude Code

These are **two separate steps** — run both, in order:

```text
/plugin marketplace add Goldziher/basemind   # 1. register the marketplace (makes the plugin available)
/plugin install basemind@basemind            # 2. install the plugin (registers the MCP server)
```

Adding the marketplace alone does **not** give you any tools — it only makes the plugin available to
install. You must run the second command (or pick **Install** for the `basemind` plugin in the
`/plugin` menu) to register the MCP server. If no basemind tools appear after a restart, you almost
certainly stopped after step 1; open `/plugin`, go into the **basemind** marketplace, and **Install**
the plugin.

Restart the session after installing. The basemind binary installs automatically on first use (via
npx, uvx, or direct download with checksum verification) — no manual `cargo install` needed.

#### Statusline

To enable the live statusline, run `/bm-statusline` once. This is a one-time opt-in because **Claude
Code plugins cannot set the main statusline** — it is a platform limitation, not a basemind choice:

- The plugin manifest (`plugin.json`) has no `statusLine` field.
- A plugin-shipped `settings.json` honors only `agent` and `subagentStatusLine`; any `statusLine` key
  is silently ignored.
- Hooks communicate via stdout/stderr/exit codes only — a SessionStart hook **cannot** write to
  `~/.claude/settings.json`, so it can only _nudge_ you to run `/bm-statusline`.

`/bm-statusline` works because Claude (the agent) performs the settings edit on your behalf, writing
an **absolute** path into `~/.claude/settings.json` (`$HOME`/`~` are not expanded in the statusLine
command field). After that it persists across sessions.

Output: `◆ basemind  ●  1,247 files · 23m ago  │  47 calls · 14k saved`. Counts render bright; the
state dot is green (serve active / scan < 1 h), amber (idle or scan 1–24 h), or red (no serve and
stale index). When a document/memory/web index is present, a third segment appears: `│  312 docs ·
18 mem · 4 sites`. Narrow terminals collapse to `◆ basemind ● 1.2k · 23m │ 47c · 14k saved`.

### Any MCP client

```bash
cargo install basemind --features full --locked
```

Then add to your MCP config:

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

Supported harnesses: Claude Code · Cursor · Codex (CLI + App) · Gemini · OpenCode · Factory Droid ·
GitHub Copilot CLI · Continue · Cline. Each harness has install instructions in the
[Installation](#installation) section below.

### CLI only

```bash
basemind scan                     # index the working tree
basemind query outline path/file.rs  # inspect structure
basemind query symbol "parseQuery"   # find by name
basemind watch                    # live re-index on file change
```

---

## Why basemind, specifically

### vs grep / ripgrep

**What ripgrep does well:** blazing-fast line matching. **What it misses:**

- Grep returns 50+ hits in docs, tests, comments, variable names — agent wastes context filtering noise.
- No scope awareness: `parseQuery()` and `parseQuery` string both match; semantic signals lost.
- Every query re-scans the disk; no pre-computed structures to leverage.

basemind: semantic-quality answers at grep speed via tree-sitter + indexed call sites.

### vs vector-only RAG (LangChain / LlamaIndex DIY stacks)

**What vector RAG does well:** fuzzy document semantic search. **What it misses:**

- Pure embeddings lose exact structure — which function calls which, which class implements which interface.
- No line/column resolution — agent can't map vector hits back to code symbols.
- No git history integration — "what changed recently?" and "who wrote this?" require separate systems.

basemind: code structure + git history + vector memory + document search all in one, unified scope.

### vs context7 / openai-codex / Aider's repo-map

**What these do well:** generate code-map summaries. **What they miss:**

- Static snapshots — stale after the first edit.
- No semantic indexing — every lookup re-parses or re-scans.
- Human-focused output (markdown) instead of agent-facing structure (JSON tools).

basemind: live-updated index with sub-millisecond MCP tools, built for agents not humans.

### vs GitHub native search

**What GitHub does well:** repository-wide fuzzy text search. **What it misses:**

- Cloud-only — your code leaves the machine, latency is network-bound.
- No local-editor integration — agent can't query in-progress edits before commit.
- No cross-language polyglot support — each language's search tuned separately.

basemind: local-only, always-fresh index of your working tree, 300+ languages in one sweep.

---

## Performance

Measured on Apple Silicon, release build, `--features full`, default `eager_l2 = true`. Cold
filesystem cache adds ~50% to first scan; numbers below are warm steady-state.

### Scan throughput

| Repo | Files | Language mix | Time |
|---|---|---|---|
| tokio | 859 | Rust | 0.2 s |
| react | 7 061 | TS / JSX | 2.2 s |
| django | 7 061 | Python | 2.5 s |
| requests | 2 195 | Python | 0.7 s |
| gin | 1 217 | Go | 1.0 s |
| ripgrep | 12 851 | Rust | 4.0 s |
| ripgrep-shallow | 12 851 | Rust | 0.16 s |
| TypeScript compiler | 81 324 | TS / JS / JSON | ~22 s |

The TypeScript compiler is the worst case — 81k files scanned in 22 seconds. Most real repos sit
between tokio and ripgrep. Re-scans skip unchanged content hashes, so warm rescans on edited
working trees are typically dominated by the changed-set size, not repo size.

### Per-tool MCP latency

Against the 81k-file TypeScript index:

<!-- markdownlint-disable MD013 -->

| Latency | Tools |
|---|---|
| < 1 ms | `outline`, `list_files`, `find_references`, `find_callers`, `find_implementations`, `hot_files`, `repo_info` |
| 3–6 ms | `search_symbols`, `call_graph` |
| 4–10 ms | `recent_changes`, `commits_touching`, `find_commits_by_path`, `symbol_history`, `diff_outline`, `diff_file` |
| 20–25 ms | `status` |
| 30–40 ms | `blame_file`, `blame_symbol` |
| 40–200 ms | `workspace_grep` |
| ~200 ms | `search_documents` |
| 350–600 ms | `working_tree_status` |

<!-- markdownlint-enable MD013 -->

basemind preloads L1 outlines into RAM on `serve` start, so code-map queries hit no disk. The Fjall
LSM inverted index handles ref/caller/impl lookups without scanning blobs. Git tools track `gix`
walk cost; Fjall-backed tools dominate only on enormous histories.

---

## Configuration

Full config lives at `schema/basemind-config-v1.schema.json`. Minimal example:

```toml
# .basemind/basemind.toml
file_watch_glob = "**/*.{rs,ts,tsx,py,go}"
eager_l2 = true

[documents]
enabled = true
```

Per-query MCP overrides:

```json
{
  "query": "what does kreuzberg do?",
  "reranker_enabled": true,
  "reranker_preset": "bge-reranker-base"
}
```

Environment variables map mechanically: `--llm-api-key` ↔ `BASEMIND_LLM_API_KEY`. Every MCP tool
accepts per-query overrides that win over file/env/CLI layers.

---

## Architecture

<details>
<summary><strong>Pipeline &amp; module breakdown</strong></summary>

```text
source files
  → tree-sitter parsers (300+ langs, pack name dispatch)
  → L1 outlines + L2 calls + L3 structural hash blobs (content-addressed)
  → Fjall LSM inverted index (symbols / calls / imports / impls)
  → MCP server (rmcp) + documents pipeline (kreuzberg) → LanceDB
  → 32 MCP tools across 8 coding-agent harnesses
```

- **Scanner** (`src/scanner.rs`) — rayon-parallel walker over the gitignore-aware file set.
  Extracts L1 (symbols + imports + implementations), L2 (calls + docs), L3 (structural hashes)
  per file.
- **Content-addressed blobs** (`src/store.rs`) — msgpack at
  `.basemind/blobs/<blake3>.{l1,l2,l3}.msgpack`. Two files with identical content share the
  same blob.
- **Inverted index** (`src/index/`) — Fjall LSM keyspace at
  `.basemind/views/<view>/index.fjall/`. Nine partitions drive symbol search, references,
  implementations, and dependents.
- **MCP surface** (`src/mcp/`) — stdio JSON-RPC via rmcp. Tool descriptions are routing surface
  for agents; semantics stated honestly (substring vs prefix, scope-aware vs name-only, capped).
- **Git layer** (`src/git.rs`, `src/git_cache.rs`) — gix-backed blame, log, diff, status.
  Sha-keyed disk cache makes warm queries free.

</details>

---

## Installation

<!-- markdownlint-disable MD013 -->

| Channel | Command | Platforms | Features |
|---|---|---|---|
| Homebrew | `brew install Goldziher/tap/basemind` | macOS, Linux | base |
| npm | `npm install -g basemind` | any Node 14+ platform | base |
| pip | `pip install basemind` | any Python 3.8+ platform | base |
| cargo | `cargo install basemind --locked` | any Rust platform | base |
| cargo (full) | `cargo install basemind --features full --locked` | any Rust platform | documents + memory + crawl |
| GH releases | Download binary from [releases](https://github.com/Goldziher/basemind/releases) | macOS · Linux · Windows | base |

<!-- markdownlint-enable MD013 -->

<details>
<summary><strong>Harness-specific setup</strong></summary>

| Harness | Install command |
|---|---|
| Claude Code | `/plugin marketplace add Goldziher/basemind` then `/plugin install basemind@basemind` |
| Cursor | See Cursor docs for plugin install flow; `basemind` manifest at `.cursor-plugin/plugin.json` |
| Codex CLI | `/plugins` then search for `basemind` |
| Codex App | Plugins panel → Coding category → basemind → `+` |
| Gemini CLI | `gemini extensions install https://github.com/Goldziher/basemind` |
| OpenCode | Add `{ "plugin": ["basemind-opencode@latest"] }` to `opencode.json` |
| Factory Droid | `droid plugin --help` (manifest at `.claude-plugin/marketplace.json`) |
| GitHub Copilot CLI | `copilot plugin --help` (same manifest) |
| Generic MCP | See "Any MCP client" section above |

</details>

---

## Differentiators

- **Content-addressed dedup** — Blake3-hashed L1/L2/L3 blobs deduplicated across files and
  views. Edit a file, rescan, skip unchanged hashes.
- **Secret-masking `SecretString`** — api_key fields redacted in Debug/Display/Serialize.
  Tracing spans and panic messages never leak the value.
- **Provenance ledger** — every config value's origin tracked via `ConfigSource` (MCP > CLI >
  env > TOML > defaults). Audit trail for debugging.
- **Schema-driven config** — Rust types in `src/config/` drive
  `schema/basemind-config-v1.schema.json` via `schemars`; snapshot is asserted byte-equal.
  Config is code.
- **Zero-system-dep ONNX** — `ort-bundled` ships the runtime in the binary. No
  `apt install onnxruntime`, no system complexity.

---

## Project state

- **Real-OSS hardening:** `tests/harden.rs` runs the full tool sweep against 8 upstream repos
  (ripgrep, tokio, TypeScript, React, Django, requests, gin, ripgrep-shallow) on every release.
  Canary assertions catch regressions.
- **[CHANGELOG.md](CHANGELOG.md)** — release history and migration notes.
- **[Contributing guide](CONTRIBUTING.md)** — development workflow: `task setup`, `task check`,
  `task build`. Pre-commit hooks via [prek](https://github.com/j178/prek).
- **[License: MIT](LICENSE)**
