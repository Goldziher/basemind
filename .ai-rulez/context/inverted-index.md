---
priority: high
---

# Inverted Index (Fjall)

The Fjall LSM keyspace is the secondary index over the canonical msgpack blob store.
Lives at `.gitmind/views/<view>/index.fjall/`. Source: `src/index/{mod,keys,writer}.rs`.

## Keyspaces

| Keyspace | Purpose |
|---|---|
| `meta` | Constants (e.g. `schema_ver`). |
| `symbols_by_path` | Per-file outline lookups. |
| `symbols_by_name` | `name`-prefix range scans for symbol search. |
| `calls_by_path` | Per-file call lookups. |
| `calls_by_callee` | `callee`-prefix range scans — drives `find_references`. |
| `imports_by_module` | Future fast-path for `dependents`. |
| `embeddings` | Reserved for vector search; empty today. |

## Key shape

All composite keys are length-prefixed (`u16:len ‖ bytes`). Length prefixes guarantee that a `Foo`
prefix never spills into `Foobar`. Concrete shapes live in `src/index/keys.rs` with `*_prefix` +
`parse_*` companions for every encoder.

## Operational invariants

- **Schema version**: `INDEX_SCHEMA_VER` constant in `src/index/mod.rs`; persisted in the `meta` keyspace.
  On mismatch, open wipes `index.fjall/` and the next scan rebuilds from the msgpack blobs.
- **Read-before-write delete**: `IndexWriter::upsert_file` reads existing primary entries first to
  derive secondary-index keys, then stages all deletes + inserts in one atomic Fjall batch. The only
  correct way to update on re-scan.
- **Per-file commit**: `IndexWriter::commit` runs at the end of each `process_file`. Fjall handles
  cross-thread locking across the rayon workers.
- **`SymbolKind` ordinals are stable**: `symbol_kind_byte()` in `keys.rs` maps each variant to a
  fixed `u8`. Reordering would silently miscategorize cached entries; new variants extend the tail.

## Vector search

The `embeddings` partition is reserved for future use. Planned backend: `usearch` (HNSW + SIMD)
for KNN; `fastembed-rs` or an external API for embedding generation. Not implemented this
iteration — design constraint only.
