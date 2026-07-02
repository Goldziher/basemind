---
priority: high
---

# Crate Layout

Basemind is a single Rust crate that builds a CLI binary (`basemind`) and exposes its internals as a library. Two binaries-in-one: `basemind scan` indexes a workspace into `.basemind/`; `basemind serve` runs the MCP stdio server.

## `src/`

- `lib.rs` — public re-exports.
- `main.rs` — CLI entry (`scan`, `serve`).
- `scanner.rs` — rayon-parallel file walker; orchestrates per-file extraction and writes blobs + index.
- `scanner_docs.rs` — document-tier scan (PDF / Office / HTML → LanceDB) when `--features documents`.
- `store.rs` — content-addressed msgpack blob store at `.basemind/blobs/<hash>.{l1,l2,l3}.msgpack`. Holds the `IndexDb` handle.
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
- `mcp/` — MCP server:
  - `mod.rs` — server bootstrap.
  - `tools.rs` + `tools_admin.rs` / `tools_git.rs` / `tools_memory.rs` / `tools_web.rs` — `#[tool]` shims (thin wrappers; 1000-line cap each).
  - `helpers.rs` + `helpers_documents.rs` / `helpers_calls.rs` / `helpers_graph.rs` / `helpers_grep.rs` / `helpers_impls.rs` / `helpers_web.rs` — tool bodies, sliced by area.
  - `memory.rs` — `search_documents` + `memory_*` over LanceDB.
  - `types.rs` + `types_documents.rs` / `types_graph.rs` / `types_impls.rs` — `JsonSchema`-derived request/response structs.
  - `cursor.rs`, `savings.rs`, `telemetry.rs` — pagination cursors, token-savings heuristics, telemetry sink.
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

#### `.basemind/` (created at scan time)

- `blobs/<hash>.{l1,l2,l3}.msgpack` — content-addressed extraction blobs (dedup across files / views).
- `views/<view>/index.fjall/` — Fjall LSM tree (the secondary index over those blobs).

#### Other

- `schema/` — JSON Schemas (e.g. `basemind-config-v1.schema.json`), regenerated from the Rust types via `schemars` and asserted byte-equal by `tests/config_schema.rs`. Never hand-edit.
- `build.rs` — code generation (tree-sitter query bundles; `rerun-if-changed` plumbing).
- `poly.toml` — poly checks: typos, markdown, cargo fmt/clippy/sort/machete/deny, rustdoc-lint, rust-max-lines (1000-line cap).
- `deny.toml` — cargo-deny license / source allow-list.
- `Cargo.toml` — single-binary crate; key deps: `fjall`, `gix`, `ahash`, `memchr`, `rayon`, `rmcp`, `rmp-serde`, `tree-sitter*`.
