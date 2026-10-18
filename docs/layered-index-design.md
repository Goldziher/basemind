# Layered index: design and implementation plan

- **Status:** Design accepted, not implemented
- **Date:** 2026-10-18
- **Decision record:** [ADR-0016](adr/0016-shared-base-index-and-worktree-overlays.md)
- **Tracking issue:** #87
- **Target release:** 0.31.0 (phases A2–A8; A0/A1 shipped in 0.30.0)

This document is the executable plan behind ADR-0016. The ADR records *what* was decided and *why*;
this file records *how* — the on-disk layout, the data model, the exact read-merge rules per keyspace,
the write path, the correctness gate, and the file-by-file phase breakdown. Symbol names and paths are
as of 0.30.0.

## 1. Problem

Each git worktree is its own workspace: the store key is blake3 of the canonical worktree root
(`workspace_key` in `src/store_layout.rs`), so every worktree gets a full Fjall index under
`cache/workspaces/<ws_key>/views/working/index.fjall/`. On a large monorepo (89k files, 65k indexed)
one working-view index is ~2.9 GB, of which 2.6–2.8 GB is the Fjall tree. Only the content-addressed
blobs (`cache/blobs/`) and the git-history index are already shared.

`store_seed.rs` clones a sibling's `views/working` with `clonefile`, but Fjall compaction rewrites the
pages, so the copy diverges into a full copy. With 33 worktrees that is ~95 GB, and every cold worktree
costs a 17–33 min scan peaking at 6–7 GB RSS.

The goal: per-worktree disk and scan cost proportional to the worktree's diff, with query results
**identical** to a full standalone index.

## 2. Current state (0.30.0)

What already exists and is reused:

- **Cache layout** (`src/store_layout.rs`): `cache/blobs/` (global, content-addressed),
  `cache/workspaces/<workspace_key>/` (per worktree: `views/<view>/index.fjall/`, `index.msgpack`,
  `status.json`, `.lock`), and `cache/repos/<repo_group_key>/` (per clone).
- **Repo group** (`repo_group_key`): blake3 of the canonical **main worktree root** — the parent of the
  shared git common dir — so every linked worktree of one clone maps to the same group. Already used
  for the shared memory store (`cache/repos/<group>/memory.fjall`, phase A0).
- **Keyspaces** (`src/index/mod.rs`): `meta`, `symbols_by_path`, `symbols_by_name`, `calls_by_path`,
  `calls_by_callee` (+ `callee_names` NameDict), `imports_by_module`, `imports_by_path`,
  `implementations_by_trait` (+ `trait_names`), `implementations_by_path`, `refs_by_def`,
  `refs_by_path`, `import_targets`, `importers_by_probe`, `code_bm25_postings`, `code_bm25_by_path`,
  `grep_bloom`, `embeddings`. Key encoders live in `src/index/keys.rs`, every one length-prefixed
  (`u16:len ‖ bytes`) with a `*_prefix` / `parse_*` companion.
- **Write path** (`src/index/writer.rs`): `upsert_file` is read-before-write — it reads the existing
  primary rows to derive the secondary keys, then stages all deletes + inserts in one atomic Fjall
  batch; `remove_file` mirrors it; `commit` runs per file.
- **Rev scan** (`ScanSource::Rev` in `src/scanner.rs`): `Repo::list_paths_rev(sha)` enumerates a
  revision's paths and `Repo::read_blob_at_rev_with_oid` reads its bytes — the base builder.
- **Probe index** (A1, `import_targets` / `importers_by_probe`): the O(affected) re-stitch of importers
  a changed path can affect. This is what makes the overlay's `S_refs` small.
- **Blob-backed reads**: `code outline` / `expand`, the `graph` builders and `grep`'s candidate
  enumeration go through `L1Cache` / `FileIndexView` and read content-addressed blobs keyed by the file
  map's `hash_hex`, **not** Fjall. Only the *path → hash* map needs merging for these; the blob layer is
  already shared and content-addressed. `code symbols` uses the Fjall `symbols_by_name` fast path
  (`search/exact.rs` → `IndexDb::symbols_by_name_lookup`), so it is covered by the merge table below.

## 3. Target on-disk layout

```text
cache/
  blobs/                                  # global, content-addressed (unchanged)
  repos/<group>/
    memory.fjall                          # A0, shipped
    lance/                                # A7: shared documents / doc_links / code_chunks
    base/
      gen-<n>/
        index.fjall/                      # immutable base index (published read-only)
        index.msgpack                     # base manifest: RelPath -> hash_hex
        base.json                         # { generation, commit, built_unix, file_count, bytes }
    overlays/<ws_key>/
      overlay.fjall/                      # changed-path rows only
      delta.msgpack                       # DeltaManifest (below)
      overlay.json                        # { base_generation, updated_unix, counts }
  workspaces/<ws_key>/                    # legacy standalone index; still the fallback
    views/working/index.fjall/
```

`<ws_key>` is the existing `workspace_key(root)` — the overlay is per worktree, the base per group.
The legacy `workspaces/` tree is kept while `[index] layered = false` and as the promotion target.

## 4. Data model

### 4.1 `DeltaManifest` (`delta.msgpack`)

```rust
struct DeltaManifest {
    schema_ver: u16,
    base_generation: u32,
    base_commit: Option<String>,      // the base's `origin/HEAD` (or main HEAD) oid
    added: AHashMap<RelPath, String>, // path -> content hash
    modified: AHashMap<RelPath, String>,
    deleted: AHashSet<RelPath>,
    reimported: AHashSet<RelPath>,    // importers re-stitched by the A1 probe pass
}
```

`added ∪ modified ∪ deleted` is the worktree's diff against the base; `reimported` are unchanged files
whose resolved edges the probe pass rewrote.

### 4.2 Shadow sets (derived, not stored)

- **`S_path`** = `added ∪ modified ∪ deleted` — the overlay owns every key whose path is in this set.
- **`S_refs`** = `S_path ∪ reimported` — the overlay owns every resolved-edge key whose *use* path is in
  this set.
- **`D_hash`** = `{ base_manifest[p] : p ∈ S_path ∩ base_manifest }` — base content hashes no longer live
  in the worktree (drives the BM25 corpus-stat correction and `grep_bloom` sharing).

### 4.3 Merged file map

The in-RAM `Index.files` (`store.rs`) in layered mode is `base_manifest ⊕ delta`: overlay hashes win for
`S_path`, base entries survive otherwise. This single merged map feeds `L1Cache` / `FileIndexView`, so
every blob-backed read (`outline`, `symbols`, `expand`, `grep`'s candidate enumeration) works unchanged
— the blobs are already content-addressed and shared.

## 5. Read merge rules

### 5.1 The ordering invariant (the crux)

Every scan the read path performs is a **prefix scan within one name** (`symbols_by_name`,
`calls_by_callee`, `imports_by_module`, `implementations_by_trait`) or a **path-prefix scan**
(`*_by_path`). Within a single prefix the key suffix is `(kind?, path, start_byte)` and is ordered
identically in the base and the overlay, because both use the same encoders in `keys.rs`.

Therefore a **k-way merge of the base and overlay streams by raw key bytes, with base rows filtered by
the shadow sets, is byte-for-byte the standalone stream.** `limit`, `scan_cap = limit * 8`, and every
cursor are therefore identical — no cursor re-derivation is needed.

> Note: the keys are length-prefixed, so *across* different names the partition is **not**
> lexicographically ordered. Nothing in the read path relies on cross-name order (the name scans
> iterate the `NameDict` and prefix-scan one name at a time), so the merge never needs it. A future
> full-partition ordered scan would need its own treatment.

### 5.2 Per-keyspace rules

| Keyspace | Merge |
|---|---|
| `symbols_by_path`, `calls_by_path`, `imports_by_path`, `implementations_by_path` | path in `S_path` → overlay only; else base only |
| `symbols_by_name`, `calls_by_callee`, `imports_by_module`, `implementations_by_trait` | prefix-scan both; drop base rows whose key's `path` ∈ `S_path` |
| `refs_by_def` | prefix-scan both; drop base rows whose `def_path` ∈ `S_path` **or** `use_path` ∈ `S_refs` |
| `refs_by_path` | prefix-scan both; drop base rows whose `use_path` ∈ `S_refs` |
| `import_targets` | overlay only for importers ∈ `S_refs`; else base |
| `importers_by_probe` | drop base rows whose probed-path ∈ `S_path`; else base |
| `code_bm25_postings` | drop base postings for chunk_ids of paths ∈ `S_path`, then overlay postings. Enumerate a shadowed path's base chunk_ids via `code_bm25_by_path_prefix(p)` — the existing forward companion, no extra index |
| `code_bm25_by_path` | path in `S_path` → overlay only |
| `meta` (BM25 corpus stats) | `n = base_n − Σ_{p ∈ S_path ∩ base} chunks(p) + overlay_n`; `total_len` likewise. Must be exact: it changes scores |
| `callee_names`, `trait_names` (NameDict) | base ∪ overlay. Over-inclusion is safe (an extra empty range scan); under-inclusion is not |
| `grep_bloom` | keyed by content hash and **shared**: one row per distinct content, so a base row is correct for any path whose hash matches. See A2 |
| `embeddings` | reserved; no rows today |
| `memory_by_key`, `memory_archive`, `proposals` | already per-group (`memory.fjall`), outside the view |

`importers_by_probe` and `import_targets` are resolution bookkeeping; they must merge consistently with
`refs_*` or the next incremental re-stitch will miss or duplicate edges.

## 6. Write path

Per scan of a worktree (layered mode):

1. **Candidates** = `git diff --name-only <base_commit>` ∪ untracked ∪ basemind-only (files the base
   manifest lacks). Only candidates are read and hashed; everything else reuses the base manifest hash.
2. Extract changed files exactly as today (`process_file`), writing blobs (shared) and staging
   `overlay.fjall` rows through `IndexWriter` pointed at the overlay database.
3. Run the A1 probe pass against the **merged** view, staging re-stitched importers into the overlay
   and recording them in `reimported`.
4. Write `delta.msgpack` + `overlay.json` atomically (last).
5. **Never** write the base.

The overlay is written by the same `IndexWriter` code path; the only difference is which `IndexDb` handle
it holds. This keeps `upsert_file`'s read-before-write and per-file commit semantics intact.

## 7. Generations, rebase, promotion, GC

- **Generation build**: from `origin/HEAD` (falling back to the main worktree HEAD) via `ScanSource::Rev`
  into a fresh `base/gen-<n>/`. New generations advance by a CoW clone of the previous generation plus
  `git diff-tree C_n..C_n+1` applied through the incremental writer.
- **Cadence**: when the base has drifted by ≥2% of files, or daily.
- **Publication**: a generation is written to `gen-<n>.tmp/`, fsynced, then renamed to `gen-<n>/` and
  marked read-only, so the daemon keeps one handle and one block cache per group and readers never see a
  half-built generation.
- **Retention**: at most three generations; the oldest is dropped once no overlay references it.
- **Rebase**: when an overlay's delta exceeds 15% of files or 25% of bytes, rebase it onto the newest
  generation (recompute the diff, drop the shadow sets that fall away).
- **Promotion**: when the worktree is on the base commit and the delta is empty, or when rebasing would
  not shrink the delta, promote the overlay to a standalone index (today's model) and delete the overlay.
- **GC**: `store_gc` learns the `repos/<group>/base` and `overlays/` trees. `base/` is rebuildable
  (deletable); `overlays/` is rebuildable from the worktree; `memory.fjall` and `lance/` are not
  (never listed). Orphaned overlays (worktree gone) are reaped via the `workspace.json` liveness check
  already used for `workspaces/`.

## 8. `IndexView` trait (A3)

The read path today takes a concrete `&IndexDb` (or a cloned handle) and calls Fjall directly. A3
introduces a trait so the layered implementation can slot in with **no behaviour change**:

```rust
/// Which partition a scan targets. Mirrors the `IndexDb` keyspace fields.
pub enum Partition { SymbolsByName, CallsByCallee, RefsByDef, CodeBm25Postings, /* … */ }

/// A read-only view over an index: standalone today, base+overlay after A5.
pub trait IndexView: Send + Sync {
    fn get(&self, p: Partition, key: &[u8]) -> Result<Option<Vec<u8>>, IndexError>;

    /// Visit rows whose key starts with `prefix`, in encoded-key order, stopping on `Break`.
    fn scan_prefix(
        &self,
        p: Partition,
        prefix: &[u8],
        visit: &mut dyn FnMut(&[u8], &[u8]) -> std::ops::ControlFlow<()>,
    ) -> Result<(), IndexError>;

    /// Visit every row of the partition in encoded-key order.
    fn scan_all(
        &self,
        p: Partition,
        visit: &mut dyn FnMut(&[u8], &[u8]) -> std::ops::ControlFlow<()>,
    ) -> Result<(), IndexError>;
}
```

A **visitor** (not an `Iterator`) is deliberate: Fjall scans borrow the keyspace, and a boxed iterator
would force lifetime plumbing through every call site; the visitor also gives the early-stop the
`scan_cap` budget needs. `StandaloneView(&IndexDb)` is a direct delegation; `LayeredView` (A5) does the
k-way merge of §5.

`IndexRoute` (Local / Host / Daemon) and `index_read_against` take `&dyn IndexView`. The wire
(`IndexReadQuery`) is unchanged by A3 — the daemon constructs its own view.

## 9. Correctness gate (A5)

A `proptest` that, for a fixture repo:

1. Builds a **standalone** full index of worktree *i* (the reference).
2. Builds a **base + overlay** index of worktree *i* on the same content.
3. Applies random edits — modify / add / delete / rename / duplicate / barrel file / `tsconfig` /
   Python package layout — across three worktrees, rescanning the layered index.
4. Asserts, for **every** `code` / `graph` / `git` mode: identical result rows, identical `total`, and
   identical cursors across a full pagination walk, versus a fresh standalone index of the same content.

This is the gate that must be green before `layered` can default to `true` (A8).

## 10. Phases

Each phase is its own commit, lands behind `[index] layered = false`, and keeps CI green.

### A2 — re-key `grep_bloom` by content hash

- `src/index/grep_bloom.rs`: key = hex content hash; row = `version(1) | bits` (drop the
  `(size, mtime_ns)` stamp — the hash *is* the identity). `build_row(bytes)`,
  `grep_bloom_row(hash)`, `grep_bloom_current(hash)`, and
  `grep_skip_verdicts(root, entries, needle)` where an entry carries `(rel, hash, size, mtime_ns)`, so
  the live-file guard uses the **indexed** stamp from `FileEntry` instead of the row's own.
- `src/index/writer.rs`: `upsert_grep_bloom(hash, row)`; `remove_file` stops deleting the now-shared row.
- `src/index/mod.rs`: `sweep_grep_bloom(&live_hashes)` (drop rows no live file references), run in the
  full-scan purge; bump `INDEX_PARTITION_REVISION` 6 → 7.
- `src/scanner_index_batch.rs`: stage by hash; `restamp_grep_bloom` / `backfill_grep_bloom` key on hash.
- `src/mcp/l1_cache.rs`: add `mtime_ns` to `FileMeta`.
- `src/mcp/helpers_grep.rs`, `src/mcp/index_route.rs`, `src/mcp/index_read.rs`,
  `src/comms/index_read_proto.rs`: carry the entries through `IndexReadQuery::GrepBloom`.
- Tests: round-trip, prefix isolation, sweep, two-paths-same-content dedup.

### A3 — `IndexView` / `StandaloneView`

- New `src/index/view.rs`: `Partition`, `trait IndexView`, `StandaloneView`.
- Thread `&dyn IndexView` through `IndexRoute`, `index_read_against`, `query.rs`, `mcp/helpers*.rs`,
  `search/*`. Pure refactor — the existing suite is the gate.

### A4 — repo-group layout, base builder, migration

- `src/store_layout.rs`: `repos/<group>/base/gen-<n>/` and `repos/<group>/overlays/<ws_key>/` path
  helpers + `base.json` / `overlay.json` sidecars.
- Base builder: `ScanSource::Rev` over `origin/HEAD` into `gen-0/`, published read-only.
- Migration: a workspace whose only index is the legacy standalone becomes base + overlay on first
  layered open; `store_seed.rs` stays for now.

### A5 — overlay write + read merge (the core)

- `DeltaManifest` + shadow sets; `LayeredView` implementing §5; overlay write through `IndexWriter`.
- Candidates = `git diff --name-only <base>` ∪ untracked ∪ basemind-only; blob reuse.
- BM25 corpus-stat correction; NameDict union.
- Rebase / promotion thresholds.
- **Gate:** the §9 proptest.

### A6 — generations, rebase, promotion, GC

- Generation advance (CoW clone + `diff-tree`), publication-by-rename, three-generation retention,
  overlay GC wired into `store_gc`, orphan reap.

### A7 — shared Lance tables

- `documents` / `doc_links` re-keyed to `(scope, content_hash)`; queries over-fetch and filter by live
  hashes. `code_chunks` is already content-addressed.
- Documented deviation: top-k overlap with a per-worktree table stays ≥0.95.

### A8 — layered by default

- Flip `[index] layered` to `true`; delete `store_seed.rs`; CHANGELOG + ADR status update.

## 11. Risks and open questions

- **Merge completeness.** The gate (§9) is the only thing that can catch a missed shadow rule. It is
  deliberately exhaustive across modes and cursors.
- **Ordering.** §5.1's invariant depends on the encoders staying order-consistent within a prefix; a
  future key change must preserve it, and a cross-name ordered scan would need its own merge.
- **BM25 corpus stats.** Scores are sensitive to `n` / `total_len`; the correction must be exact, and
  `code_bm25_by_path` is the only forward map to a shadowed path's chunk_ids.
- **Daemon vs index-less host.** The hosted stack's index-less store cannot see `refs_*` edges
  (`src/mcp/host.rs`); layered mode must behave identically in that mode, which the smoke suite covers.
- **Concurrency.** The base is published read-only so one handle serves a group; the overlay is
  single-writer per worktree, matching today's per-workspace `.lock`.
- **Schema.** The overlay and base share `INDEX_SCHEMA_VER`; a bump wipes and rebuilds both from source,
  which is the same migration story as today.
