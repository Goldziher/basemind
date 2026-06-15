---
priority: high
---

# Performance Discipline

Basemind is a hot-path scanner that processes tens of thousands of files in seconds. Hot-path code lives in `src/scanner.rs`, `src/extract/`, `src/store.rs`, `src/index/`, and `src/mcp/helpers*.rs`. Apply these patterns by default; deviate only with measurement.

- Use `ahash::AHashMap` / `AHashSet`, never `std::collections::HashMap` on the scanner / extract / index paths. The crate has a workspace dep — reuse it.
- Use `memchr::memmem::Finder` for substring matching, not `str::contains` or `str::find`. The Finder is built once and reused.
- Hex encoding/decoding goes through the zero-copy flow in `src/store.rs` — do not introduce `String::from_utf8(hex::encode(...).into_bytes())` round-trips.
- Cache MCP import lookups; do not re-parse imports per query.
- Avoid `.clone()` on `String` / `Vec<u8>` in the scanner inner loop. If you need a borrow, pass `&str` / `&[u8]`. If you need ownership later, defer the clone to the boundary.
- Tree-sitter parsers / queries are expensive to build — reuse them through the parser pool in `src/extract/`. Never construct one per file.
- Rayon `par_iter` is the parallelization unit — do not spawn raw threads, do not use `tokio::spawn` in the scanner.
- Before optimizing further, run the harden harness (`tests/harden.rs`) and capture the scan-time delta against the baseline in the `harden-harness` context.
