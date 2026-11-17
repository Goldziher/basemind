---
type: Concept
title: Code Style
x-ai-rulez:
  kind: context
  id: code-style
  metadata:
    priority: high
---

# Code Style

Project-specific conventions baked into context so they ship into every AI tool's config.

## Module layout

- One concern per file. When `src/mcp/tools.rs` approached the 1000-line cap, the bodies were extracted to `helpers.rs` and then sliced again by area (`helpers_documents.rs`, `helpers_calls.rs`, `helpers_grep.rs`, `helpers_impls.rs`). Match that shape when adding a new tool area: a verb tool is a params struct in `types_tools.rs`, a thin shim in `tools*.rs`, and a body in `helpers_*.rs`.
- 1000-line cap on `src/**/*.rs` enforced by `tests/max_lines.rs` under `cargo test`. Refactor by extracting helpers or types, never by lifting the cap.
- Tests sit in `tests/<area>_smoke.rs` files (one per area) plus the integration harness at `tests/harden.rs`.

### Performance

- `ahash` for hash maps, never `std::collections::HashMap` on hot paths.
- `memchr::memmem::Finder` for substring matching — built once and reused.
- Zero-copy hex encoding via `src/store.rs`; do not introduce `String::from_utf8(hex::encode(...))` round-trips.
- Cache MCP import lookups; never re-parse imports per query.
- Rayon `par_iter` is the scanner parallelism unit; no `tokio::spawn` in the scanner.

#### Testing & lints

- TDD: failing test first when the change is observable via the public API or MCP surface.
- Clippy strict (`-D warnings`); silence only with a one-line `//` justification.
- `poly lint .` covers: typos, markdown line length, cargo fmt / clippy / sort / machete / deny, rustdoc-lint.

#### Codegen

- `build.rs` + `schema/*.json` are the codegen surface. Hand-editing generated files is forbidden.
- Rust config types in `src/config/` drive the JSON Schema via `schemars` derives; the snapshot at `schema/basemind-config-v1.schema.json` is asserted byte-equal by `tests/config_schema.rs`. Regenerate with `cargo test --test config_schema -- --ignored regenerate_schema`.

#### Commits

- Conventional Commit prefixes (`feat:`, `fix:`, `perf:`, `chore:`, `refactor:`). Match the style in `git log`.
- Body explains *why*, not *what*. Mention schema bumps explicitly.
