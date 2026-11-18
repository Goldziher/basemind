---
type: Concept
title: Crate Layout
x-ai-rulez:
  kind: context
  id: crate-layout
  metadata:
    priority: high
---

# Crate Layout

Basemind is a single Rust crate that builds a CLI binary (`basemind`) and exposes its internals as a library. Two binaries-in-one: `basemind scan` indexes a workspace into the machine-global cache (`~/.local/share/basemind/`, override `BASEMIND_DATA_HOME`); `basemind serve` runs the MCP stdio server.

## `src/`

- `lib.rs` — public re-exports.
- `main.rs` — CLI entry (`scan`, `serve`).
- `scanner.rs` — rayon-parallel file walker; orchestrates per-file extraction and writes blobs + index.
- `scanner_docs.rs` — document-tier scan (PDF / Office / HTML → LanceDB) when `--features documents`.
- `store.rs` — content-addressed msgpack blob store at `~/.local/share/basemind/cache/blobs/v<schema>/<hash>.{fm,chunk,rref,doc}.msgpack`. Holds the `IndexDb` handle.
- `index/` — Fjall-backed secondary index (`mod.rs`, `keys.rs`, `writer.rs`).
- `extract/` — tree-sitter extraction tiers:
  - `l1.rs` — outlines (symbols, signatures, imports, docs).
  - `l2.rs` — call sites (callee, byte offset, line/col).
  - `l3.rs` — structural hash of symbol bodies.
  - `doc.rs` — xberg integration; `FileMapDoc` (plus `keywords` / `entities` / `summary` on the documents path).
- `config/` — schema-driven config:
  - `v1.rs` — top-level `ConfigV1` + `LlmConfig`, schemars-derived.
  - `documents.rs` — `DocumentsConfig` sub-tree, `ApiKey` enum, `SecretString`.
  - `overrides.rs` — `DocumentsCliOverrides` (backs clap `#[command(flatten)]` + MCP `#[serde(flatten)]`).
  - `layered.rs` — `merge_layers` (Mcp > Cli > Env > File > Default).
  - `source.rs` — `ConfigSource` + `ProvenanceMap` ledger.
- `mcp/` — MCP server: fifteen verb tools, one per job with an exact schema (ADR-0018), plus the three deferred agent tools (`agent_post`, `agent_inbox`, `agent_threads`) in a `comms` build (ADR-0021).
  - `mod.rs` — server bootstrap.
  - `types_tools.rs` — one `deny_unknown_fields` params struct per verb tool, and the `tool_args!` table that names each tool and builds `VERB_TOOLS`.
  - `strict.rs`, `tool_contract.rs` — `Strict<T>` argument validation (unknown field → did-you-mean + accepted list), `answer` (failures become `isError` results), and `finalize_router` (always-load marker, schema compaction).
  - `adapter.rs`, `cursor_bind.rs` — run an engine operation for a shim or a CLI command: a blocking thread, `EngineError` → `McpError`, the cursor bound to its query, the telemetry row.
  - `tools.rs` (`code_*`, `find_files`) + `tools_git.rs` (`git_*`) / `tools_memory.rs` (`docs_search`, `memory_*`) / `tools_admin.rs` (`index_*`) — one thin `#[tool]` shim per tool plus the `<tool>_call` method the CLI also runs (1000-line cap each). `tools_comms.rs` holds the agent tools.
  - `helpers.rs`, `helpers_admin.rs` (`index_status`, `index_rescan`), `helpers_telemetry.rs` — what is left on the MCP side; every other tool body is in `engine/`.
  - `savings.rs`, `telemetry.rs` — token-savings heuristics (keyed by tool name, with the pre-ADR-0018 `code:outline`-style keys mapped onto them), telemetry sink.
- `engine/` — the engine API (ADR-0039), which names no rmcp type: `ops/<tool>.rs` (one synchronous `impl Workspace` method per verb tool, returning `Result<_, EngineError>`), `git/` (the git tools), `types/<area>.rs` (their responses), `memory/`, `refs/`, and the contract pieces every body uses (`budget.rs`, `cursor.rs`, `filters.rs`, `freshness.rs`, `pick.rs`, `files.rs`, `error.rs`).
- `query.rs` — read-side helpers shared between MCP tools and the CLI.
- `git.rs` + `git_cache.rs` — `gix`-backed history / blame / churn.
- `path.rs` — `RelPath` byte-precise repo-relative paths.
- `lang.rs` — `LangId = &'static str` (the tree-sitter-language-pack pack name), parser pool, query cache, override-then-TSLP-fallback `try_get_query`.
- `queries/<pack-name>.scm` — hand-written extraction queries (`;; section: symbols / imports / calls / docs`) that win over the upstream `tags.scm` fallback.
- `render.rs`, `hashing.rs`, `watcher.rs`, `config/` — supporting modules.

### `tests/`

- `mcp_smoke.rs` — synthetic-fixture MCP contract.
- `harden.rs` — clones 8 real OSS repos and exercises the full tool sweep with canary assertions.
- `git_smoke.rs` / `git_cache_smoke.rs` / `scan_smoke.rs` / `schema_bump.rs` / `config_schema.rs` — focused smoke tests.
- `fixtures/` — small synthetic repos for unit tests.

#### Global cache (`~/.local/share/basemind/`, created at scan time)

- `cache/blobs/v<schema>/<hash>.{fm,chunk,rref,doc}.msgpack` — content-addressed extraction blobs (dedup across files / views), one namespace per blob schema so two installed versions never share a blob.
- `cache/groups/<group>/index.fjall/` — Fjall LSM tree (the secondary index over those blobs), one per repository group (ADR-0028, ADR-0036): every worktree and view of a repository shares it, held by one process under the group's `.lock`. Beside it the group's document retrieval stores in `lance/`.
- `cache/workspaces/<workspace_key>/views/<view>/` — per view, `index.msgpack` (the file map) and `pending-vectors.msgpack` (paths owed vector rows).

#### Other

- `schema/` — JSON Schemas (e.g. `basemind-config-v1.schema.json`), regenerated from the Rust types via `schemars` and asserted byte-equal by `tests/config_schema.rs`. Never hand-edit.
- `build.rs` — code generation (tree-sitter query bundles; `rerun-if-changed` plumbing).
- `poly.toml` — poly checks: typos, markdown, cargo fmt/clippy/sort/machete/deny, rustdoc-lint. The 1000-line cap is `tests/max_lines.rs`.
- `deny.toml` — cargo-deny license / source allow-list.
- `Cargo.toml` — single-binary crate; key deps: `fjall`, `gix`, `ahash`, `memchr`, `rayon`, `rmcp`, `rmp-serde`, `tree-sitter*`.
