---
priority: high
---

# Crate Layout

Basemind is a single Rust crate that builds a CLI binary (`basemind`) and exposes its internals as a library. Two binaries-in-one: `basemind scan` indexes a workspace into `.basemind/`; `basemind serve` runs the MCP stdio server.

## `src/`

- `lib.rs` — public re-exports.
- `main.rs` — CLI entry (`scan`, `serve`).
- `scanner.rs` — rayon-parallel file walker; orchestrates per-file extraction and writes blobs + index.
- `store.rs` — content-addressed msgpack blob store at `.basemind/blobs/<hash>.{l1,l2,l3}.msgpack`. Holds the `IndexDb` handle.
- `index/` — Fjall-backed secondary index (`mod.rs`, `keys.rs`, `writer.rs`).
- `extract/` — tree-sitter extraction tiers:
  - `l1.rs` — outlines (symbols, signatures, imports, docs).
  - `l2.rs` — call sites (callee, byte offset, line/col).
  - `l3.rs` — structural hash of symbol bodies.
- `mcp/` — MCP server:
  - `mod.rs` — server bootstrap.
  - `tools.rs` — `#[tool]` methods (thin wrappers; ~1000-line cap).
  - `helpers.rs` — tool bodies, shared scan/decode helpers.
  - `types.rs` — request/response structs with `JsonSchema`.
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

- `schema/` — JSON Schemas (e.g. `basemind-config-v1.schema.json`). Hand-edited; `build.rs` validates round-trip with the Rust types.
- `build.rs` — code generation (schema-derived types, tree-sitter query bundles).
- `.pre-commit-config.yaml` — prek hooks: typos, markdown, cargo fmt/clippy/sort/machete/deny, rustdoc-lint, rust-max-lines (1000-line cap).
- `deny.toml` — cargo-deny license / source allow-list.
- `Cargo.toml` — single-binary crate; key deps: `fjall`, `gix`, `ahash`, `memchr`, `rayon`, `rmcp`, `rmp-serde`, `tree-sitter*`.
