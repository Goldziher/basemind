---
type: Concept
title: Inverted Index
x-ai-rulez:
  kind: context
  id: inverted-index
  metadata:
    priority: high
---

# Inverted Index (Fjall)

The Fjall LSM keyspace is the secondary index over the canonical msgpack blob store. One database
per repository group (ADR-0028) at `<data dir>/cache/groups/<group>/index.fjall/`, shared by every
view of every worktree of the group; a view's own rows sit under its 4-byte view id. Source:
`src/index/{mod,keys,writer,group,scoped}.rs`.

## Keyspaces

| Keyspace | Purpose |
|---|---|
| `meta` | Constants (e.g. `schema_ver`). |
| `symbols_by_path` | One content's symbols, keyed by content key; shared by the group. |
| `symbols_by_name` | `name`-prefix range scans for symbol search; shared by the group. |
| `calls_by_path` | One content's call sites, keyed by content key; shared by the group. |
| `calls_by_callee` | `callee`-prefix range scans — drives `code_refs`; shared by the group. |
| `code_bm25_postings` | BM25 postings shared by the group: `term ‖ key` → packed `ordinal ‖ tf ‖ doclen` per chunk. |
| `code_bm25_by_path` | A view's per-file BM25 forward rows; their terms name the postings to retire. |
| `code_bm25_holders` | `key ‖ view id ‖ path` per holder of a BM25 key; a posting goes once none names its key. |
| `embeddings` | Reserved for vector search; empty today. |

### Key shape

All composite keys are length-prefixed (`u16:len ‖ bytes`). Length prefixes guarantee that a `Foo`
prefix never spills into `Foobar`. Concrete shapes live in `src/index/keys.rs` with `*_prefix` +
`parse_*` companions for every encoder.

The symbol and call rows are keyed by a 16-byte content key (content hash, language, extract
epoch) in place of the path (`src/index/keys_content.rs`): `symbols_by_path` / `calls_by_path` are
`ckey ‖ start`, the name-led rows `u16:len(name) ‖ name ‖ [kind] ‖ ckey ‖ start`. A view reads them
through its live keys (`src/index/live_keys.rs`), the content key each file map entry records with
the paths holding it; a name scan's hit key is the row key followed by one holder's path.

#### Operational invariants

- **Schema version**: `INDEX_SCHEMA_VER` constant in `src/index/mod.rs`; persisted in the `meta` keyspace.
  On mismatch, open wipes `index.fjall/` and the next scan rebuilds from the msgpack blobs.
- **Read-before-write delete**: `IndexWriter::upsert_file` reads existing primary entries first to
  derive secondary-index keys, then stages all deletes + inserts in one atomic Fjall batch. The only
  correct way to update on re-scan.
- **Per-file commit**: `IndexWriter::commit` runs at the end of each `process_file`. Fjall handles
  cross-thread locking across the rayon workers.
- **Group sweep** (ADR-0041, `src/index/group_sweep.rs`): at the end of a full pass it retires views
  whose view directory is gone and, when the group owes it, deletes the symbol and call rows of
  contents no file map names and no writer has pending. Shared rows are never deleted by a scan.
- **`SymbolKind` ordinals are stable**: `symbol_kind_byte()` in `keys.rs` maps each variant to a
  fixed `u8`. Reordering would silently miscategorize cached entries; new variants extend the tail.

#### Vector search

The `embeddings` partition is reserved for future use. Planned backend: `usearch` (HNSW + SIMD)
for KNN; `fastembed-rs` or an external API for embedding generation. Not implemented this
iteration — design constraint only.
