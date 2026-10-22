# ADR-0016: Shared base index with per-worktree overlays

- **Status:** Accepted
- **Date:** 2026-10-18
- **Deciders:** Na'aman Hirschfeld
- **Related:** ADR-0012, issue #87, [implementation plan](../layered-index-design.md)

## Context

Each git worktree is its own workspace: the store key is blake3 of the canonical root
(`src/store_layout.rs`), so every worktree gets a full Fjall index. On a large monorepo (89k files, 65k
indexed) one working-view index is ~2.9 GB, of which the Fjall is 2.6–2.8 GB; only the content-addressed
blobs and `git-history.fjall` are shared. `store_seed.rs` clones a sibling's `views/working` with
clonefile, but compaction rewrites the pages, so the copies drift into full copies. With 33 worktrees that
is ~95 GB, and every cold worktree costs a 17–33 min scan peaking at 6–7 GB RSS. The per-worktree disk and
scan cost should be proportional to the worktree's diff, with query results identical to a full index.

## Decision

Group worktrees by blake3 of the canonical git common dir and split the index into an immutable **base**
and a per-worktree **overlay**:

- **Base** — an index of `origin/HEAD` (falling back to the main worktree HEAD) built from the committed
  tree, so it never contains uncommitted edits. The tree is exported to a scratch directory and scanned
  like a working tree, because the rev scan alone does not run the resolve pass. Generations advance by
  a CoW clone plus `git diff-tree C_n..C_n+1` applied through the incremental writer when files have
  drifted by 2% or more, or daily; at most three are live (two by default; a third only when the disk cap allows),
  each published once and never logically rewritten (a Fjall generation still replays its journal and compacts when
  opened, see *Lifecycle*).
  The base is read through a `BaseReader`, so its storage is pluggable (*Base storage*). With a Fjall
  base the daemon keeps one handle and one block cache per group (fjall admits one process per
  directory, so other processes reach the base through the daemon); an immutable mmapped base can be
  opened by every process.
- **Overlay** — `delta.msgpack` records each path as added, modified or deleted relative to the base
  manifest by content hash; `overlay.fjall` holds rows only for changed paths and re-stitched importers.
  Shadow sets `S_path` (delta paths), `S_refs` (`S_path` plus re-stitched importers) and `D_hash` (base
  hashes no longer live in the worktree) drive the merge.
- **Read merge** — every per-name keyspace already carries the path in its key, so the overlay merges with
  the base preserving order, totals and cursors: `*_by_path` take the overlay for a path in `S_path`;
  `symbols_by_name`, `calls_by_callee`, `imports_by_module` and `implementations_by_trait` drop base rows
  whose path is shadowed; `refs_by_*` drop base rows whose use-path is in `S_refs`;
  `code_bm25_postings` drop base rows whose hash is in `D_hash`; the BM25 `meta` is base + overlay −
  shadowed `code_bm25_by_path`. `grep_bloom` is re-keyed by content hash and shared.
- **Write path** — candidates are `git diff --name-only C_base` ∪ untracked files ∪ basemind-only files;
  only candidates are hashed, and blobs are reused. An `IndexView` trait with standalone and layered
  implementations exposes `prefix`/`range`/`get`/`iter`, and `IndexRoute` / `index_read_against` take
  `&dyn IndexView`. If the delta exceeds 15% of files or 25% of bytes the overlay rebases onto a newer
  generation or is promoted to a standalone index (today's model).
- **Lance** — tables are shared per group: `code_chunks` is already content-addressed and
  `documents`/`doc_links` are re-keyed to `(scope, content_hash)`, with queries over-fetching and filtering
  by live hashes. Documented deviation: top-k overlap with a per-worktree table stays ≥0.95.

## Specification

Everything below is normative for A2–A8. Paths and symbols are as of 0.30.0 (`bd1f0a5b`), plus A2 (`efe41b76`)
and A3 (`26be3d56`), both on main. Where the code contradicts the text of issue #87 or the summary above, the code wins and the correction is listed first.

### Corrections to the issue text

| Claim in #87 | What the code does | Consequence |
|---|---|---|
| "Rev scan builds the base" (`ScanSource::Rev`) | `scan_with_cancel` runs the resolve pass only for `ScanSource::WorkingTree` (`scanner.rs`, `if matches!(source, ScanSource::WorkingTree)`), and the resolver reads the filesystem (`intel/resolve_pass.rs` `std::fs::read`, `intel/resolver.rs` `root.join`, `canonicalize`). A rev scan therefore writes no `refs_by_*`, `import_targets` or `importers_by_probe` rows, and the rev branch of `process_file` writes no grep bloom either. | The base builder exports the committed tree to a private scratch directory and runs a normal working-tree scan on it (A4). A virtual git-tree filesystem for the resolver was rejected: it duplicates the resolver's I/O surface for a transient 1-3 GB export. |
| "Published read-only so the daemon keeps one handle" | Fjall's directory lock is exclusive even for a read-only open (`Store::open_read_only_no_index`, `mcp/index_route.rs` module doc). Read-only is a logical property, not something the OS enforces on a shared fjall. | For a Fjall base, one process per machine can hold a given generation. The daemon is that holder; every other process reaches the base through `IndexRoute::Host` / `Daemon` or degrades to `InRam`, exactly as a second `serve` does today (A4). Conditional on the base-storage prototype: an mmapped sorted-run base has no such lock (*Base storage*). |
| "`code_chunks` is already content-addressed" | `chunk_id` is `<hash_hex>:<ordinal>` (`chunk.rs`) and the sidecar blobs are content-addressed, but the Lance rows are keyed `(scope, path)` (`lance/mod.rs` `replace_code_chunks`, `delete_code_chunks`) and `scope` is the normalised `origin` URL (`git/remote.rs` `scope_key`), identical for every worktree of a clone. | Sharing a Lance table needs a new key for all three tables, not just `documents`/`doc_links` (A7). |
| A3 "shipped" (progress comment) | When the ADR was written only the ADR had shipped: `src/index/view.rs` did not exist and `IndexDb` keyspaces were read directly. A2 landed in `efe41b76` and A3 in `26be3d56`. | A3 is a prerequisite of A5 and is done, and the follow-up that made `view.rs` backend-neutral (`IndexBytes`) with default named accessors has shipped (A3 below). |
| `repo_group_key` hashes "the git common dir" | It hashes the canonical main worktree root, the parent of the common dir (`store_layout.rs` `repo_group_key`, `git/mod.rs` `main_worktree_root`). Equivalent for a normal clone; a bare clone or a moved main checkout re-keys the group. | Keep the function; record the resolved common dir in `group.json` so a re-key is diagnosable (A4). |
| BM25 `meta` is exact as "base + overlay - shadowed" | True, but `upsert_bm25_file` / `remove_bm25_file` delete every posting of the path's `chunk_id`s (`index/writer.rs` `stage_bm25_deletes_for`), and `chunk_id` embeds the content hash. Two paths with identical bytes share their postings, so removing or editing one of them deletes the postings the other still needs. | Pre-existing defect in the standalone index, not specific to layering, and it breaks the A5 equivalence gate on the "duplicate content" scenario. A5.0 fixes it first (below). |
| `main_root_of` / `repo_group_key` cover a workspace | A workspace rooted at a *subdirectory* of a repository also gets a group key (`Repo::discover` walks up), but a base built from the whole tree would not match a subdirectory scan. | Layering applies only when the canonical root equals the git workdir (the `store_seed::main_root_of` predicate); otherwise the workspace stays standalone (A4). |

### Vocabulary

- **group**: `repo_group_key(root)`; its directory is `repo_group_dir(root)` = `cache/repos/<group>/` (`store_layout.rs`).
- **wt_key**: `workspace_key(root)`; its directory is `workspace_cache_dir(root)` (unchanged, still holds `.lock`,
  `workspace.json`, `status.json`, git caches, and the `staged` / `rev-*` views, which stay standalone).
- **W**: the paths a working-tree scan would index (`scanner_candidates::walk_candidates` after `Filters`).
- **T**: the keys of the base manifest (`Index.files` of the pinned generation), i.e. the filtered paths of `tree(C_base)`.
- **H_base**: the set of content hashes in the base manifest, with a per-hash path count.
- **Shadow sets**: `S_path`, `S_refs`, `D_hash` as defined in the Decision.

### A2: re-key `grep_bloom` by content hash

The plan in #87 stands; this fixes the details the plan left open.

- **Key and row.** Key = the 64-character lowercase hex blake3 (`hashing::hex`), so two paths with identical
  bytes share one row. Row = `version(1) | bits`; `ROW_VERSION` goes 1 to 2, so every old row (path-keyed,
  17-byte header) parses as absent and reads as a candidate. `build_row(bytes)`; `GrepEntry { rel, hash_hex, size_bytes, mtime_ns }`
  (`index/grep_bloom.rs`, serde, because it crosses the daemon wire).
- **Live-file guard.** `grep_skip_verdicts(root, &[GrepEntry], needle)` skips a file only if the row for
  `entry.hash_hex` rejects the needle *and* `stat(root/rel)` equals the **indexed** `(size_bytes, mtime_ns)` of
  the entry, taken from `FileEntry` (`store.rs`) through `FileMeta` (`mcp/l1_cache.rs`, gains `mtime_ns`). A file
  whose indexed mtime is 0 (git sources) never equals a live stat and stays a candidate: `mtime_ns == 0` is never skipped.
  A file whose mtime is within 2 s of the scan's stat time is untrusted for skipping (the same-tick edit hole: an edit
  inside the filesystem's timestamp granularity can leave `(size, mtime)` unchanged), so it stays a candidate until a
  later scan restamps it.
- **Stamp refresh (not in the plan).** The stamp used to live in the row and `restamp_grep_bloom` refreshed it
  when a file was touched with unchanged content. The stamp now lives in the `FileEntry`, and
  `process_file` returned `Unchanged` without upserting one, so a touched file would have stayed a permanent
  grep candidate. The unchanged-content branch of `scanner_file::process_file` therefore returns a
  `FileResult` carrying a refreshed `FileEntry` (same hash, new `size_bytes` / `mtime`) for working-tree scans.
- **Writer.** `IndexWriter::upsert_grep_bloom(hash_hex, row)` overwrites an identical row; `remove_file` no longer
  touches `grep_bloom`. `WorkerIndexBatch::{stage,restamp,backfill}_grep_bloom` key on the hash; backfill reads
  the file only when the hash has no row and drops the bytes if they no longer hash to the indexed value.
- **Sweep.** `IndexDb::sweep_grep_bloom(&live)` deletes every row whose hash is not in `live`. `scanner::scan_with_cancel`
  calls it right after the stale-entry purge with `live` = hashes of `store.index.files`, so a pass that was
  cancelled or aborted (which returns before the purge) never sweeps. Edited and deleted contents leave their
  row behind until that sweep, which is the only deletion path for a full scan. The sweep is key-only (it never decodes a
  row) and runs only for `WorkingTree` scans, because a rev scan writes no bloom rows and would sweep them all. The
  watcher-driven `scan_paths` also deletes a content row when its hash leaves the manifest (no other path holds it), so
  incremental edits do not wait for the next full pass.
- **Schema.** `INDEX_PARTITION_REVISION` stays 6 and `INDEX_SCHEMA_VER` is unchanged, so no user re-indexes. Old
  rows are migrated by `ROW_VERSION` alone: a row of another version is absent on read (the file is a candidate and
  is scanned), a one-time version sweep, guarded by a `meta` marker so it runs once per index, deletes path-keyed and
  other-version rows, and an incremental scan rebuilds the content-keyed rows (backfill reads each unchanged file once).
- **Wire.** `IndexReadQuery::GrepBloom { pattern, entries }` (`comms/index_read_proto.rs`). A client and daemon of
  different builds fail to decode, and `IndexRoute::grep_skip` already degrades that to "sweep every file".
- **Layered note.** Rows are content-addressed and never shadowed: a layered read does `overlay.get(h)` then
  `base.get(h)`; a stale base row for a dead hash is harmless because nothing asks for it.

### A3: `IndexView` (standalone only)

Shipped in `26be3d56`; the follow-ups below (a prerequisite of A4) have shipped on main.

- `src/index/view.rs`: `enum Part` naming the 16 layered keyspaces (below), and
  `trait IndexView: Send + Sync { get(Part, &[u8]); prefix(Part, &[u8]); range(Part, Bound, Bound); iter(Part); }`
  returning boxed iterators of `KvResult`, plus the named accessors that exist on `IndexDb`: `references_to`,
  `definition_of`, `import_targets_of`, `importers_probing`, `is_probed`, `import_probes_complete`,
  `symbols_by_name_lookup`, `symbols_index_is_empty`, `bm25_stats`, `grep_bloom_row`, `grep_skip_verdicts`, and
  `callee_keys_matching` / `trait_keys_matching`, which read the `NameDict` fields `callee_names` / `trait_names`
  (`pub(in crate::index)` fields of `IndexDb`, not accessors).
- `StandaloneView(&IndexDb)` delegates 1:1. `IndexRoute::Local` holds `Box<dyn IndexView>`; `index_read_against`,
  `query.rs`, `mcp/helpers_calls*.rs`, `mcp/helpers_impls.rs`, `search/{bm25,exact}.rs` and the resolve pass
  (`intel/resolve_pass.rs`, which reads `import_targets`, `importers_by_probe`, `refs_*`) take `&dyn IndexView`.
  Writes keep going through `IndexWriter` on the concrete `IndexDb`.
- **Backend-neutral values (shipped).** `KvResult` and `get` / `grep_bloom_row` in `src/index/view.rs` use the
  backend-neutral `IndexBytes` (cheap to clone, derefs to `[u8]`), so no `fjall` type appears in the trait and a
  sorted-run `BaseReader` can implement it without copying into Fjall types. Only `StandaloneView` and the Fjall base
  reader convert at their edge.
- **Named accessors are default methods (shipped).** `references_to`, `definition_of`, `import_targets_of`,
  `importers_probing`, `is_probed`, `import_probes_complete`, `symbols_by_name_lookup`, `bm25_stats`,
  `symbols_index_is_empty`, `grep_bloom_row` and `grep_skip_verdicts` are default methods written over the generic
  `get` / `prefix` / `range` layer (the `Meta` scalars through `get(Part::Meta, ..)`), so a layered view inherits them
  and cannot drift from the standalone decoding. `StandaloneView` may override a default only for speed and the
  equivalence gate proves the override equal. `callee_keys_matching` / `trait_keys_matching` are the only required
  methods: a layered view must merge them (A5, *NameDict*).
- The existing suite is the gate. Perf discipline applies: the dyn call is per iterator, not per row, but a
  p95 regression above 3% on `references` / `callers` (benches in `benches/`) switches `IndexView` to an enum
  with the two variants instead of a trait object.

### A4: repo-group layout, base generations, migration

**Config.** New `[index]` table in `config/v1.rs` (`IndexConfig`, `deny_unknown_fields`, in `ConfigV1` after
`cache`): `layered: bool`, default `false` until A8. Regenerate `schema/` with the existing
`config_schema` test. The env var `BASEMIND_INDEX_LAYERED=0|1` overrides the file (rollback lever, same pattern as
`BASEMIND_NO_SEED`). `max_group_bytes: Option<u64>` caps a group's `base/` plus `overlays/` (*Disk budget*); unset means *auto*.
`advance_stall_ms: u64`, default 2000, bounds the reader stall during *Advance*. The mode is resolved once per `Store::open`; the daemon applies a changed value when it next
opens the workspace. Layered applies to a workspace only when all hold: `layered` is true, the canonical root is the
git workdir, `Repo::discover` succeeds, a base commit resolves, and no embedding lane is enabled until A7 has
shipped (`documents.embed`, `code_search.embed`: their Lance rows are still keyed by worktree). `documents.embed`
defaults to `true` (`config/documents.rs` `default_embed`), `code_search.embed` to `false`, so out of the box layering
is off until A7 and A4–A6 apply only to users who set `documents.embed = false`. The reference-monorepo measurement for A4–A6 must
run with embeds off, and A8 (layered by default) depends on A7.

**Base storage (pluggable; prototype-first).** The base is read through a `BaseReader` behind `IndexView`; the overlay
and every mutable store stay Fjall. `base.json` carries `storage = "fjall" | "sorted-run"` and a reader is chosen per
generation from it. Owner decision: the base is prototype-first. An immutable mmapped sorted-run base, exported from
a finished Fjall generation, is built and benchmarked before A4 is implemented, and A4 must not be locked to a Fjall
base. Everything below that is Fjall-specific is marked *(Fjall option)* and is conditional on the prototype outcome:
the `index.fjall/` layout, `IndexDb::open_code_only`, the quiesce and `RwLock<Arc<IndexDb>>` in *Advance*, the
generation clone, the daemon-only access rationale, and open question 1 and the matching Consequences text about one
process per base. A mmapped base can be opened by every process, which would mostly dissolve open question 1.

*A4-prototype gate.* Export one reference-monorepo base generation to a sorted-run file set and measure, against the Fjall
generation, on the same machine, cold and warm:

| Measure | Acceptance (to be fixed by the owner from the first run) |
|---|---|
| base size on disk | no larger than the Fjall generation |
| build time (export from Fjall, and cold build) | recorded; export under 10 min |
| `references` / `callers` p95 through the `BaseReader` | within the 3% / 10% budgets of A3 and the measurement plan |
| open cost (time to first answer, RSS) | below Fjall open incl. journal replay |
| multi-process open | N processes open one generation concurrently, no lock, each reads correctly |

If sorted-run meets the gate it becomes the default storage and the *(Fjall option)* items are dropped from the
spec; otherwise Fjall stays and the text applies as written. The `IndexView` and merge contract below are written
against `BaseReader` and hold for both.

**Layout.**

```text
cache/repos/<group>/
  memory.fjall/                       A0, untouched
  group.json                          {format, main_root, common_dir, created_unix}
  base.lock                           flock; serialises build / advance / retire / GC for the group
  base/
    gen-<n>/{index.fjall/ | index.run/, index.msgpack, base.json}   published (index.fjall/ is the Fjall option)
    .tmp-<pid>-<nanos>/               builder staging (incl. checkout/); swept after 1 h when unlocked
    .retired-<n>/                     renamed out of selection, deleted once fjall can be locked
    last-failure.json                 {error, unix}; a failed build is not retried for 10 min
  overlays/<wt_key>/{overlay.fjall/, delta.msgpack, overlay.json}
  lance/                              A7
cache/workspaces/<wt_key>/            unchanged; `workspace.json` gains `repo_group` and `index_mode`
```

`WorkspaceMarker` (`store_layout.rs`) has no `deny_unknown_fields`, so the extra fields are invisible to older
binaries. `store_layout.rs` gains `base_dir`, `generation_dir`, `overlay_dir`, `BASE_LOCK_FILE` and friends; none of
`repos/` is ever listed by the budget eviction today and that changes in A6.

**`base.json` (format 1, JSON, written last).**

```json
{ "format": 1, "generation": 7, "commit": "<sha>", "tree": "<tree oid>", "source_ref": "origin/HEAD",
  "built_from_generation": 6, "built_unix": 0, "basemind_version": "0.31.0",
  "index_schema_ver": 0, "blob_schema_ver": 0, "min_extract_epoch": 1,
  "scan_digest": "<blake3>", "embed_policy": "<digest>", "files": 65123, "bytes": 0, "probes_complete": true }
```

A generation is *ready* iff `base.json` parses with `format == 1`. It is *compatible* iff `index_schema_ver ==
INDEX_SCHEMA_VER`, `blob_schema_ver == extract::SCHEMA_VER`, `min_extract_epoch >= extract::EXTRACT_EPOCH` and
`scan_digest` equals the digest of the running config. `scan_digest` is the blake3 of the canonical JSON of the config
subtrees that change which rows a scan writes: `scan.{include,exclude,max_file_bytes}`, `languages`, `code_intel`,
`code_search.enabled`. `scan.extra_roots` is excluded: files outside the repository are always overlay-only. An
incompatible generation is ignored and collected, the same wipe-on-mismatch story as `INDEX_SCHEMA_VER`
(`schema-and-blob-compat` rule).

**`index.msgpack` of a generation** is the existing `store::Index` (`files`, `doc_files` empty, `embed_policy`),
with every `FileEntry.mtime` normalised to 0. The base is code-tier only; the document tier stays in the overlay
until A7.

**Base commit.** `refs/remotes/origin/HEAD` resolved with `Repo::resolve_rev`; if absent or unresolvable, `HEAD` of
the main worktree (`Repo::main_worktree_root`). basemind never fetches.

**Builder (cold).** Holds `base.lock` (new `LockHolder::BaseBuild`; contention reports the holder from `.lock.meta`).

1. Allocate `n = 1 + max(existing gen / retired numbers)`; create `.tmp-*`.
2. Export `tree(C)` to `.tmp-*/checkout/` via new `Repo::export_tree(rev, dest)` (regular files only; symlinks and
   submodules are skipped, matching the scanner's filters). Peak disk 1-3 GB, deleted before publish.
3. Run `scanner::scan` with `ScanSource::WorkingTree` rooted at the checkout, `EmbedMode` off, into a private
   `Store` whose view dir is `.tmp-*/`; this runs the wholesale resolve pass and sets `import_probe_ver`.
4. Normalise mtimes, rewrite `index.msgpack` atomically, flush every keyspace (the `rotate_memtable_and_wait`
   loop in `IndexDb::open_inner`) and close fjall *(Fjall option)*. Closing makes the SSTs and the journal durable; the
   builder then fsyncs every file under `.tmp-*`, so nothing published depends on page-cache state.
5. Write `base.json` last: write `base.json.tmp`, fsync it, rename it to `base.json`, fsync `.tmp-*`.
6. `rename(.tmp-*, gen-<n>)` (atomic, same filesystem), then fsync the parent `base/` directory so the rename is
   durable, then update `CURRENT` (tmp + rename, an advisory hint only). Every fsync is `F_FULLFSYNC` on macOS
   (plain `fsync` does not flush the drive cache there; `src/store.rs` uses `sync_all` only, so this is new).
   A crash before the rename leaves a `.tmp-*`; after it, a complete generation. A generation without a parseable
   `base.json` is never ready.

Readers never trust `CURRENT`; they list `base/gen-*` and read `base.json`. Blobs written by an aborted build stay
valid (content-addressed), so a restart re-runs mostly fjall writes. A disk-full or poisoned fjall (`IndexDb::is_poisoned`,
`fault.rs`, #79) aborts the build, removes `.tmp-*`, and writes `last-failure.json`.

**Racing builders.** Two worktrees that both find no ready generation both try `base.lock` non-blocking. The loser
does not build: it re-checks `base/` every 2 s, answers queries from the `InRam` projection with the existing
partial-scan health notice naming the winner, and takes over (sweeping stale `.tmp-*`) if the flock is released
without a ready generation, which is what a crash of the winner looks like. At most one build per group is ever in
flight.

**Bootstrap from an existing standalone index (the migration fast path).** If the group has no ready generation but
a workspace has a compatible standalone `views/working` (`store_seed::find_source` picks the source: main worktree
first, else the most files; its `.lock` must be acquirable by us, the same rule as `seed_working_view`), the builder
clones it (`store_seed::clone_dir`, CoW with a bounded plain copy) into `.tmp-*`, vets it (`vet_clone`), and corrects
it to `tree(C)` with the *advance* procedure below, treating every path where the source differs from `C`
(`Repo::status_against(C)` plus untracked plus basemind-only files, which are removed) as the diff. Cost is O(diff),
not a rescan, and the first base costs one CoW clone instead of 17-33 min.

**Advance (new generation from an old one).** Under `base.lock`, for the newest ready generation `k` and target `m`:

1. *(Fjall option)* Quiesce `k`: if this process is the daemon it takes the write side of the group's
   `RwLock<Arc<IndexDb>>`, drains, and drops its handle; otherwise it tries to open `k`'s fjall and aborts the advance
   on `Locked`. A live fjall is never copied (the rule in the `store_seed.rs` module doc). Readers of the group stall
   while the write side is held. The stall is bounded by a budget, `[index] advance_stall_ms` (default 2000), covering
   the drain of in-flight queries, the CoW clone (O(files)) and the reopen of `k` (open cost, which includes
   journal replay and the `rotate_memtable_and_wait` flush in `open_inner`, see *Lifecycle*). Exceeding it aborts the
   advance, reopens `k`, and retries after a backoff; readers are never stalled past the budget. The default is sized
   from the reference monorepo: a CoW clone of its 1.7 GB, 102-file index takes 0.36 s on APFS, so 2 s covers drain, clone and reopen
   with headroom while staying under an interactive tool-call timeout; an advance is background work, so an abort
   costs only a retry. A sorted-run base needs
   no quiesce: its files are immutable, so the clone is a hardlink or CoW copy and no reader is touched.
2. Clone `gen-k` to `.tmp-*` and, for Fjall, reopen the daemon handle immediately.
3. `Repo::diff_trees(C_k, C_m)` (new, gix tree diff; a rename is a delete plus an add) filtered through `Filters`.
4. Export only the added/modified blobs plus the unchanged files the resolve pass probes (A1's `probed_only`
   expansion) to `.tmp-*/checkout/`, apply with `scan_paths_with_observer` through the normal writer, run the
   incremental resolve pass, then steps 4-5 of the cold build.
If the filesystem has no CoW and free space is below twice the generation size, the advance is skipped and retried
later (*Disk budget*).

**Lifecycle (what "published" means).** A published generation is never rewritten by basemind's logic, but a Fjall
generation is not read-only on disk: opening it replays the journal and compacts, and `IndexDb::open_inner` calls
`rotate_memtable_and_wait` on every keyspace (`src/index/mod.rs`; best-effort, a failure only logs). So every open of a
Fjall generation can write, a CoW clone of it un-shares as compaction rewrites pages, and *Advance* must quiesce the
daemon handle before cloning. The builder's flush loop leaves the journal empty, which bounds replay at the next open
to nothing for a freshly published generation. A sorted-run generation has none of this and is read-only in fact.

**Selection.** An overlay pins `base_generation`. On open without a usable pin (first open, pin retired,
incompatible) choose among compatible ready generations the one with the smallest candidate count (`|K|` below,
computed once per generation, at most three git status calls), ties to the newest. A pin is kept while its
generation exists and is compatible.

**Opening the base.** *(Fjall option)* `IndexDb::open_code_only(gen_dir)`: like `open_tolerating_held_memory` with the memory
store always detached, because the base must not take the group's exclusive `memory.fjall` lock. The daemon's
`src/comms/workspace_pool.rs` keeps one `Arc<BaseHandle>` per `(group, generation)`; every pooled workspace of the group shares
it and its block cache. A non-daemon process uses `open_index_with_retry`'s budget and then treats `Locked` like a
lost lock today: it routes through `IndexRoute::Daemon` if a daemon answers, else `InRam`. Open question 1.

**Migration of a workspace (standalone to layered), one-way, under the workspace `.lock`.** One workspace at a time per
machine (a machine-wide try-lock; the others stay standalone and retry on their next open).

1. Check the free-space precondition (*Disk budget*); resolve or build a base (above).
2. Compute the overlay (A5, "Building the overlay") from the live tree.
3. Verify: for 200 random paths plus every path of `S_path`, compare the merged rows of each layered keyspace with
   the standalone index (`IndexView` makes this one function); any difference aborts, removes the overlay, and keeps
   the standalone view. Verify cannot see a defect the standalone index and its clone share, which is why the BM25
   layout stamp below is checked first.
4. Write `overlay.json` with `state = "ready"`, set `index_mode = "layered"` and `repo_group` in `workspace.json`,
   rename `views/working` to `views/working.migrated`, and delete it right after verify and the commit above succeed
   (a crash between the rename and the delete leaves a restorable index; the next open finishes the delete). The
   transient cost is the old index kept until then, about +2.9 GB for the one workspace being migrated.
   `store_seed::seed_working_view` is skipped for layered-eligible workspaces.

**Damaged-index inheritance.** `vet_clone` (`store_seed.rs`) checks only `msgpack` and the Fjall `schema_ver`, so a
standalone index that already lost postings for duplicate content (the defect A5.0 fixes) would be cloned into the
base and verify would compare against the same defect. `meta` therefore gains a `bm25_layout` stamp (written by A5.0's
fixed writer). `vet_clone` requires the stamp to equal the current layout; a missing or older stamp rejects the
clone as a base source, and a standalone index with an old stamp does a one-time postings re-derive from the
content-addressed chunk blobs on its next scan (the `by_path` rows hold the term list without frequencies, so they
cannot rebuild the postings). Only then is it eligible as a clone source.

**Disk budget.** A group holds up to 3 live generations (1.7 GB each on the reference monorepo today), and compaction un-shares the CoW clones,
so the realistic footprint is the sum, not one generation plus deltas. `[index] max_group_bytes` caps `base/` plus
`overlays/`. Its default, *auto*, is 4x the newest ready generation's size: two live generations, the transient export
plus clone of a build, and the overlays (each holds changed paths only). A fixed byte default would be wrong at both
ends: too small for a repo whose single generation exceeds it, and far above need for a small repo. On the reference monorepo (1.7 GB
generation) auto is about 7 GB, against 33 x 1.7 GB of standalone indexes today. Retention is therefore 2 generations
by default (newest plus the one pinned overlays are rebasing off), 3 only when the cap allows. Build, advance and migrate each check, under `base.lock`, that free space on
the volume is at least the generation size plus the cap on the transient (export plus clone) and that the group stays
under the cap, else they are skipped, a `last-failure.json`-style marker is written, and `doctor` says why. When disk is
tight the newest generation replaces the oldest instead of the group keeping 2 (the oldest goes through the *Liveness*
rules, so a pinned one is never deleted from under a reader).

### A5: overlay write path and the read merge

**A5.0 (prerequisite, standalone fix).** Postings are shared by content hash, so a path-driven delete must not
remove them while another path holds the hash. A refcount handed to `upsert_bm25_file` / `remove_bm25_file` from
`Store.index.files` is rejected: refcounts computed while parallel `WorkerIndexBatch` staging runs race the serial apply
pass (a count taken before a sibling path's batch lands is stale). Instead the writer's per-path delete of
`code_bm25_postings` is dropped (the `code_bm25_by_path` forward rows of the path are still deleted) and a
**hash-liveness sweep** next to `sweep_grep_bloom_rows` (`scanner.rs`) removes, after the stale-entry purge of a
`WorkingTree` scan, every posting whose `chunk_id` hash is not in the hash set of `store.index.files`. This is
symmetric with A2: both sweeps run from the post-pass state, are key-only for postings (the hash is parsed from the
key, `keys::parse_code_bm25_posting_chunk_id`) and tolerate leftovers until the next full pass. The watcher's
`scan_paths` deletes the postings of a hash that left the manifest, using the post-pass hash set. The same sweep is the
layered `D_hash` rule applied to a standalone index.

`N` semantics, pinned: `N` and `total_len` come from `code_bm25_by_path`, which has one row per `(path, chunk_id)`, so
`N` counts path-chunks (two paths with identical content count twice). `df(term)` counts `code_bm25_postings` rows,
which are keyed by `chunk_id`, so it counts unique chunks (once per distinct content). The asymmetry is intended: it is
what a standalone index computes today and what the layered formula (below) reproduces. The equivalence test asserts it
explicitly with a duplicate-content scenario (`N` twice, `df` once) in both modes.

Red tests on `main` today: two paths, identical content, remove one, the other's postings survive. `meta` gets a
`bm25_layout` stamp written by this fix; see *Damaged-index inheritance* (A4).

**`delta.msgpack` (format 1, MessagePack named, `rmp_serde`, same atomic tmp + rename as `Store::flush`).**

```rust
struct Delta {
    format: u16,                                   // 1
    seq: u64,                                      // monotonically increasing per commit
    base_generation: u32, base_commit: String,
    entries: BTreeMap<RelPath, DeltaEntry>,        // S_path
    restitched: BTreeSet<RelPath>,                 // S_refs minus S_path
    doc_entries: BTreeMap<RelPath, DocEntry>,      // the whole document tier until A7
    doc_tombstones: AHashMap<RelPath, DocTombstone>,
    embed_policy: String,
}
enum DeltaEntry { Added(FileEntry), Modified(FileEntry), Deleted }
```

`Added` is a path not in T; `Modified` is a path in T whose hash differs; `Deleted` is a path in T not in W. A path
whose live hash equals its base hash is never in the delta (a revert removes the entry and its overlay rows). A
version mismatch of `format` discards the overlay (it is a cache). Derived and never persisted, rebuilt at open in
O(delta): `S_path = keys(entries)`, `S_refs = S_path U restitched`, and
`D_hash = { base hash h of p in S_path } \ { h : base still has an unshadowed path with h } \ { hashes of Added/Modified }`,
computed from the per-hash path counts of H_base.

**`overlay.json` (format 1).** `{ format, worktree_key, root, base_generation, base_commit, base_scan_digest,
index_schema_ver, head, state: "ready"|"dirty", delta_seq, changed_files, changed_bytes, updated_unix }`.

**Crash safety.** A commit is: (1) set `state = "dirty"`; (2) one fjall batch containing the rows and `meta.delta_seq =
seq`; (3) write `delta.msgpack` with the same `seq` (the commit point, as `index.msgpack` is today); (4) `state =
"ready"`. On open, `dirty`, or `meta.delta_seq != delta.seq`, or a base pin that is missing or incompatible, discards
the overlay and rebuilds it. The overlay is a cache; blobs are global, so a rebuild costs O(diff), not O(repo).

**Building the overlay (full pass).** `K`, the candidates, is `{ p in W : p not in T }` U `{ p in W : p in D }` U
`{ p in W : git neither tracks nor reports p }` (basemind-only files: gitignored but admitted by `scan.include`, and
`scan.extra_roots` files), where `D` is the set of tracked paths that differ from `tree(C_base)` in the index or the
worktree, from new `Repo::status_against(tree_sha)` (gix status with `head_tree`), whose untracked part is added to
`K`. Only `K` is read and hashed. Each `p in K` becomes `Added`, `Modified`, or nothing (equal hash); each `p in T \ W` is
`Deleted`. Then the rows of every `Added` / `Modified` path are written through the ordinary `IndexWriter` into the
overlay fjall, and the incremental resolve pass (A1) runs with reads from the merged view and writes to the overlay:
the importers it re-stitches form `restitched`. `Store.index.files` is the *effective* manifest (base overlaid with
the delta), so `store.lookup`, `MapCache::build`, the blob GC and every existing caller keep working; only
`Store::flush` differs and persists the delta, driven by a `dirty_paths` set, with a full diff only at open. The
stale purge in `scan_with_cancel` is replaced in layered mode by the `Deleted` computation above.

**Watcher and `scan_paths`.** Events go through `scan_paths_with_observer` unchanged. For each path the post-state
is classified against T with the same four cases (revert, modify, add, delete) and the delta, overlay rows and
`restitched` are updated in one commit (the crash protocol above). The `.git` directory is already excluded; a
`HEAD` / index change is not a watcher event today, so a branch switch is picked up by the next full pass, which
the daemon schedules whenever `git rev-parse HEAD` of the worktree changed since `overlay.json.head`.

**Concurrency.** The overlay writer is whoever holds the workspace `.lock` (`acquire_lock_as`), the same single
writer as today (the daemon when running). The base is read-only to everyone but the builder. A layered reader loads
`Arc<Shadow>` (the three sets) and opens its base and overlay iterators under a short read lock; the writer takes the
write lock only to commit the batch and swap the `Arc<Shadow>`, so a reader never sees a path in both layers or in
neither. Fjall iterators are snapshots, so the lock is not held while iterating.

**Merge contract.** Merged order is plain byte order of the raw key (both layers use the same encoders in
`index/keys*.rs`); keys never collide across layers because a base row is dropped exactly when the overlay may hold a
row for the same shadow unit. Tombstones, snapshots and scalars are specified here once.

- **Tombstones are masks, not rows.** Overlay `get` internally distinguishes *tombstone* (the key's shadow unit is
  masked, so the answer is the overlay's, and absence there is final) from *absent* (not masked, so fall through to the
  base). The mask is the shadow set of the table below; at an equal key the overlay shadows the base in `prefix` /
  `range` / `iter`. No tombstone rows are stored.
- **Inverted keyspaces use a per-path mask.** `calls_by_callee`, `symbols_by_name`, `refs_by_def`,
  `implementations_by_trait`, `imports_by_module` and `importers_by_probe` hold the path inside the key, not as a
  prefix, so a delete cannot be a range delete. Rejected: per-key tombstones derived by walking the `*_by_path`
  forward keyspaces (it costs a base walk per deleted path and writes a row per key). Chosen: the shadowed-path set the
  ADR already defines (`S_path`, and `S_refs` for the ref and probe keyspaces; a `Deleted` path is in `S_path` with no
  overlay rows). Each base row of such a keyspace is dropped when the path parsed from its key by the keyspace's
  `Part` parser (`keys::parse_symbol_by_name`, `parse_call_by_callee`, `parse_ref_by_def` use-path,
  `parse_impl_by_trait`, `parse_import_by_module`, `parse_importer_by_probe`) is in the mask. Forward `*_by_path`
  keyspaces use the cheaper prefix check.
- **NameDict.** `callee_keys_matching` and `trait_keys_matching` return a key-ordered (`len`, then bytes), deduplicated
  union of the base and overlay dictionaries, one per layer (the dictionary is a superset filter, `name_dict.rs`, so a name that survives only in a
  shadowed base row yields a harmless empty scan). The merged iterator dedups equal names across layers and honours
  `cursor_after` and `lower` on the merged order.
- **Snapshot / pin handle.** A query takes one `ViewSnapshot { Arc<Shadow>, base reader pin, overlay iterator
  snapshot }` and every `get` / `prefix` / `range` of that query goes through it, so the two layers and the mask are
  read at one consistent point even if the writer commits or a rebase swaps the overlay mid-query. The pin keeps the
  generation (and its overlay directory) alive until the snapshot drops (A6, *Liveness*).
- **Meta scalars.** `code_bm25_n`, `code_bm25_total_len` and the import-probe completeness flag are precomputed
  scalars, so they need a decision rather than a row merge. BM25: effective value = base scalar + overlay delta, where
  the delta is `N_overlay - |base by_path rows of S_path|` (and the same for `total_len`) with the shadowed term computed
  from the base `code_bm25_by_path` rows of `S_path` at open and on each `S_path` change. This is the same formula as
  the BM25 text under *Totals, caps and cursors*; the overlay's own `code_bm25_n` / `code_bm25_total_len` hold
  `N_overlay` / `total_len_overlay` (its own rows only), not corpus totals. `recompute_bm25_stats` on an overlay
  therefore sweeps only overlay rows. `import_probes_complete`: base value (a generation is published only complete)
  AND the overlay's own flag (cleared while the overlay's resolve pass is incomplete).
- **Visited-row budget.** `scan_cap` caps hits, not rows examined (`mcp/helpers_calls_scan.rs`). A filtered scan whose
  mask drops many base rows (a widely edited name) can examine far more rows than it yields, so the merged iterator
  carries a visited-row budget (default `scan_cap * 64`, at least 128k rows given the `scan_cap` floor of 2000, about
  an estimated 0.1 s of fjall prefix iteration, not measured, so a heavily shadowed name degrades to partial rather
  than stalling); when it is hit the scan stops, returns what it has and sets the existing partial flag (`total_is_partial`). The equivalence gate runs with the budget unbounded and a separate test
  proves the partial flag.

**Merge rule per partition (every keyspace opened by `IndexDb::open_inner`).**

| Keyspace | Key (shadow unit) | Overlay holds | Base row dropped when | Notes |
|---|---|---|---|---|
| `meta` | string keys | `delta_seq`, overlay-only `code_bm25_n`, `code_bm25_total_len` | never merged as rows | `schema_ver`: both must equal `INDEX_SCHEMA_VER`. `import_probe_ver`: base AND overlay flag (merge contract). `bm25_layout`: both must equal the current layout. BM25 stats: base plus overlay delta (merge contract). |
| `symbols_by_path` | `rel` + start | rows of `Added`/`Modified` | `rel in S_path` | |
| `symbols_by_name` | name + kind + `rel` + start | same | `rel in S_path` | read by `symbols_by_name_lookup`; ordered by name then kind then `rel`, so the merge interleaves layers inside one name |
| `calls_by_path` | `rel` + start | same | `rel in S_path` | |
| `calls_by_callee` | callee + `rel` + start | same | `rel in S_path` | backs `references` / `callers`; `callee_names` dictionary = base U overlay (a superset of names is already allowed by `name_dict.rs`) |
| `imports_by_path` | `rel` + module + start | same | `rel in S_path` | written, no reader today; rule kept so the equivalence test can compare it |
| `imports_by_module` | module + `rel` + start | same | `rel in S_path` | as above |
| `implementations_by_path` | `rel` + trait + type + start | same | `rel in S_path` | |
| `implementations_by_trait` | trait + type + `rel` + start | same | `rel in S_path` | `trait_names` dictionary = base U overlay |
| `refs_by_path` | use `rel` + start | edges whose use is in `S_refs` | `use in S_refs` | `definition_of` hits one layer per use path |
| `refs_by_def` | def `rel` + start + use `rel` + start | edges whose use is in `S_refs` (def anywhere) | `use in S_refs` | def-side changes are covered because every cross-file user of a changed def is an importer that probed it and is therefore in `S_refs` (A1's exactness, which the A5 gate must prove for barrels and tsconfig) |
| `import_targets` | importer + local_start (incl. reserved manifest row) | rows of importers in `S_refs` | `importer in S_refs` | |
| `importers_by_probe` | probe hash + importer | rows of importers in `S_refs` | `importer in S_refs` | |
| `code_bm25_postings` | term + `chunk_id` (`<hash>:<ord>`) | postings of hashes not in H_base | hash of `chunk_id` in `D_hash` | a hash still live through another path keeps its base postings and is not rewritten |
| `code_bm25_by_path` | `rel` + `chunk_id` | rows of `Added`/`Modified` | `rel in S_path` | forward map; also the source of the BM25 stats correction |
| `grep_bloom` | content hash | rows for hashes with no base row | never | union by key, overlay first (A2) |
| `embeddings` | reserved, never written | none | n/a | must stay empty in both layers; asserted by the gate |
| `memory_by_key`, `memory_archive`, `proposals` | not in base or overlay | none | n/a | live in `memory.fjall` (A0); base and overlay handles are opened detached |

**Totals, caps and cursors.** The merged iterator yields exactly the `(key, value)` sequence a standalone index of
the same tree yields, filter applied before anything is counted. So `scan_calls_by_name` and `scan_impls_fjall`
(`scan_cap = max(limit * 8, 2000)`, `total_is_partial`, `has_more`) are unchanged, and a cursor is still the raw key
of the last hit as an exclusive lower bound: it is valid on both layers because each layer positions with `range`.
Cursors cross a rebase or generation change only if the merged sequence is identical, which is not guaranteed; a
cursor older than the last overlay commit is served best-effort (the existing behaviour when a standalone index is
rescanned between pages). BM25: `bm25_stats()` returns `N = N_base - |base by_path rows of S_path| + N_overlay` and
`total_len` likewise (the formula of the *Meta scalars* decision, with the subtraction computed from the shadowed base
rows at open and on each `S_path` change); `df(term)` is the count of merged postings rows, so `N` is per path-chunk
and `df` per unique chunk (A5.0).

**Manifest memory.** The effective manifest does not clone the base `FileEntry`s per worktree (65k entries times the
worktree count). The base manifest is shared as one `Arc<Index>` per `(group, generation)`, and each worktree holds a
delta map (`delta.entries` plus the derived sets); `lookup` consults the delta first and then the shared base. Code
that needs an owned `Store.index.files` map materialises it on demand, not at open.

**Gate.** `tests/layered_equivalence.rs`: a proptest over three worktrees of one group with random edit scripts
(modify, add, delete, rename, duplicate content, barrel re-export, `tsconfig` paths change, Python package
`__init__`), each compared against a freshly built standalone index of the same tree for every `Part`, every named
accessor, every MCP mode that reads the index, with totals, `has_more` and cursor round trips. The `callee_names` / `trait_names` dictionaries are exempt from
byte-equality (each layer's dictionary may hold stale names, and a layered one is a union); the gate compares the
query results they serve instead. Behind
`[index] layered = false` until green on 1000 cases.

### A6: generations, rebase, promotion, GC

**Advance triggers** (evaluated by the daemon maintenance task at start, hourly, and after each overlay full pass;
by `basemind scan` when no daemon runs): `|diff_trees(C_cur, origin/HEAD)|` is at least 2% of `base.json.files`, or
the newest generation is older than 24 h and that diff is non-empty, or an overlay breaches the rebase threshold
while the newest generation is behind `origin/HEAD`. One advance at a time per group (`base.lock`).

**Rebase and promotion** are evaluated after each overlay commit. Breach = `changed_files > 15%` of `base.json.files`
or `changed_bytes > 25%` of the base manifest's total `size_bytes`.

- If a newer compatible generation exists whose candidate count is lower, **rebase**: build `overlays/<wt>/next/` against
  it from scratch (O(candidates); blobs are reused), then swap the directories with two renames while the old
  overlay serves reads. A crash leaves a disposable `next/`.
- Otherwise, or if the rebased overlay still breaches 10% / 20%, **promote**: clone the pinned generation into
  `views/working` (`store_seed` machinery, daemon-quiesced as in *Advance*), apply the overlay's delta through
  `scan_paths` plus the incremental resolve pass, set `index_mode = "standalone"`, delete the overlay. A promoted
  workspace is never re-layered automatically (no flapping); `BASEMIND_INDEX_LAYERED=1` plus `basemind cache clear`
  is the manual way back. Promotion that cannot quiesce the base is deferred and warned in `doctor`.

**Liveness.** A generation is live iff it is the newest ready compatible one or an overlay in the group pins it. At
most three are live (two by default; a third only when the disk cap allows, see *Disk budget*); when publishing a
fourth, the overlays pinned to the oldest live generation are invalidated (their overlay dir is deleted, they rebuild on next open), except an overlay pinned by an active workspace handle or open in
the daemon: that one is never deleted from under a reader. It is rebased onto a newer generation first (the swap of
*Rebase and promotion*, so the old overlay serves reads until the new one is ready), or, if the rebase cannot run, the
oldest generation and its overlay stay live past the limit until the handle drops (the pin is released when the last
`ViewSnapshot` and handle for it are gone), and the next maintenance pass retires them. Readers meanwhile keep reading
the pinned generation and overlay unchanged and see the answers of that older tree; `doctor` reports the overstay.
Under disk pressure (*Disk budget*) the overstay is bounded by `max_group_bytes`; past it the workspace is promoted
or its overlay is dropped and the open handle is reopened. There is no eager rebase of idle worktrees. A non-live generation
is renamed to `.retired-<n>` and removed when its fjall lock can be taken *(Fjall option; a sorted-run generation is
removed once no snapshot pins it)*. An incompatible or half-published
generation (no `base.json`) is non-live. An overlay is live iff `workspaces/<wt_key>/workspace.json` names an existing
root; orphans go with `reap_orphaned_workspaces_in`, which must also remove `repos/<group>/overlays/<wt_key>`
(group read from the marker), and an overlay older than its workspace's `index_mode` flip is removed.

**Interaction with `store_gc*.rs`.**

- *Blob liveness* (`collect_referenced_hashes`, `collect_referenced_hashes_global_in`, `LiveBlobs`): also union, per
  group, the `index.msgpack` of every `base/gen-*` and `.tmp-*` whose flock is held, and every
  `overlays/*/delta.msgpack` (hashes of `Added` / `Modified`; `doc_entries` into `docs`). A manifest that is
  unreadable and not provably a foreign schema aborts the sweep (the existing safety rule); a foreign-schema
  generation is deleted under `base.lock` instead of healed. The 6-hour `BLOB_GC_GRACE` covers builds in flight.
- *Budget* (`store_gc_budget::cache_footprint`, `eviction_candidates`, `rebuildable_workspace_paths`): the footprint
  gains `repos/*/base` and `repos/*/overlays`; evicting a workspace also removes its overlay; a group's `base/` is a
  candidate only after every workspace of the group is evicted or idle past the grace window, and never while any
  of its overlays or `base.lock` is locked. `memory.fjall` stays untouched.
- *Protection* (`store_gc_protect.rs`): a workspace whose overlay pins generation `k` makes `k` part of its
  protected footprint.
- *Locking*: destructive sweeps keep the machine-wide lock in `store_gc_live.rs`; per-group work takes `base.lock`
  with a try-lock and skips a busy group.
- *Lance* rows: after A7, rows whose hash no live manifest references and that are older than the blob grace window.

### A7: shared Lance tables

- **Placement.** `repos/<group>/lance/<model_slug>-<dim>/`, one directory per `(embedding_model, dim)`, because
  `LanceStore::open` wipes the whole directory on a `LanceMeta` mismatch (`lance/mod.rs`) and a worktree with a different
  preset must not wipe its siblings; such a worktree stays standalone. `memory.lance` stays per workspace. A per-group
  `lance.lock` flock serialises write transactions across CLI and daemon.
- **Schema.** `documents`, `doc_links` and `code_chunks` gain `content_hash` (Utf8). Row identity becomes
  `(scope, content_hash, chunk_idx)`; `path` stops being part of identity. This is a column change, so it ships in a
  `RELEASE_MINOR` bump (`MEMORY_SCHEMA_VER` rule), with wipe-on-mismatch as the migration: the vectors live in the
  `.doc.msgpack` / `.chunk.msgpack` blobs, so the rebuild re-flushes rows from blobs with no model inference (as
  `purge_unembedded_*` and the seed path already rely on).
- **Writes.** The scanner inserts rows for a hash only if none exist (`content_hash IN (batch)` probe), under
  `lance.lock`. Rows are never deleted when a path changes; the A6 GC deletes unreferenced hashes.
- **Reads.** KNN returns the `k' = clamp(3k, k + 64, 4096)` nearest rows, drops rows whose hash is not in the
  effective manifest's hash set, and expands each surviving hash to every live path holding it (the reverse map is
  built at open), one hit per `(path, chunk)`. If fewer than `k` hits survive and `k'` was full, double `k'` up to
  4096 and stop. This also replaces taking the hit path from the blob (`mcp/helpers_code_search.rs` `hydrate_one`, which
  reads `chunk.path` from the content-addressed sidecar and is wrong for duplicated content). Ties are ordered by
  `(distance, path, chunk_idx)` in both modes.
- **Documented deviation.** ANN recall differs from a per-worktree table. Gate: top-10 overlap with a per-worktree
  table at least 0.95 over 200 queries per tier on the reference monorepo (`src/eval`); below that, raise `k'` or use exact search for the
  shared table.
- **Doc tier.** Until this phase the overlay carries every document (`Delta.doc_entries`); after it, documents delta
  like code. `scope` stays the normalised remote URL; it no longer separates worktrees.

### A8: layered by default

Preconditions: A5 gate green on 1000 cases, A6 GC merged, A7 shipped (A8 depends on A7, because A4 disables layering whenever an embedding lane is on and `documents.embed` defaults to `true`), measurement targets met. Then: default
`[index] layered = true`; delete `store_seed.rs`, `BASEMIND_NO_SEED`, `seed_working_view` and its callers in
`Store::open_with_holder`, and the lance-staleness sentinel `seed:rebuild-lance`; migration (A4) runs for every
standalone workspace on its next writer open.

**Rollback.** Per workspace: `[index] layered = false` or `BASEMIND_INDEX_LAYERED=0` makes the next writer open
promote (A6 procedure) or, if the base is gone, cold-scan into `views/working`; a read-only open under `false`
with an overlay present ignores the overlay, reports "no index, run basemind scan" and writes nothing. Binary
downgrade: `repos/<group>/{base,overlays,lance}` are never listed by an older GC (it only knows `memory.fjall`) and
the extra `workspace.json` fields are ignored, so an older binary simply cold-scans a fresh standalone view; the
cost is one rescan and the stranded disk, reclaimable with `basemind cache clear`. No format here changes
`INDEX_SCHEMA_VER` or `extract::SCHEMA_VER`, so a downgrade never wipes a base.

### Observability

- `doctor`, `status.json` and the stats surface expose `index_mode` (`standalone` | `layered`) and, when layered was
  not chosen, the reason: `embed_on`, `subdirectory_root`, `no_base_commit`, `config_off`, `base_building`,
  `disk_budget`, `incompatible_base`.
- For a layered workspace: the pinned generation and its `base.json` summary, overlay size and `changed_files` /
  `changed_bytes`, rebase and promote counts, base-build state (idle, building by whom, last failure), generations
  overstaying the limit, and merge-read latency (p50 / p95 of the merged iterators per `Part`).
- `tracing` events on every state transition: base build start / publish / abort, advance start / quiesce / abort,
  generation retire, overlay commit / discard, rebase, promote, migration start / verify-fail / commit.

### Acceptance criteria

Each phase is done when its line holds; the fault-injection list below is the shared gate for A4 and A6.

- **A2.** The grep-bloom, version-sweep and watcher-delete tests pass; the 20-pattern skip ratio does not worsen.
- **A3.** The existing suite passes through `IndexView`; no `fjall` type appears in the trait; the named accessors are
  default methods; `references` / `callers` p95 within the A3 budget.
- **A4.** Prototype gate evaluated and storage chosen; a cold build, a racing second builder, a bootstrap-by-clone and a
  migration each produce a generation / overlay whose merged rows equal a standalone index (A5 verify function);
  publish is crash-safe at every step; disk preconditions skip rather than fail; all with embeds off.
- **A5.** A5.0 fix and BM25 `N` / `df` equivalence pass; the equivalence gate is green on 1000 cases; overlay under 50 MB
  for a 50-file diff; visited-row budget proven.
- **A6.** Advance, rebase and promote are exercised end to end; no overlay or generation pinned by a live handle is
  deleted; GC leaves exactly the live set and protects in-flight `.tmp-*`; `max_group_bytes` is honoured; the
  fault-injection list passes.
- **A7 / A8.** Per their sections and the measurement plan.

Fault injection (each is a test that kills or fails the step and asserts the post-state and a clean re-run):

- publish crash at each step: before `base.json.tmp`, after its fsync, after its rename, after `rename(.tmp-*, gen-n)`,
  after the `base/` fsync, after `CURRENT`;
- racing builders (two processes, one winner, loser takes over after a winner crash);
- GC of `.tmp-*` and `.retired-*` (grace honoured, a locked or pinned one kept);
- quiesce and advance (stall budget exceeded, `Locked`, crash mid-clone);
- migration abort (crash between each step, restore from `views/working.migrated`) and migration-verify failure
  (injected row difference keeps the standalone view and removes the overlay).

### Measurement plan

On the reference monorepo (89k files, 65k indexed), 1 worktree then 5, standalone vs layered, same machine, cold cache and warm, with embeds off (`documents.embed = false`) until A7 ships, because A4 does not layer otherwise:

- `du -sh` of the base generation and of each overlay (target overlay < 50 MB with a typical 50-file diff);
  `du` of `grep_bloom` before and after A2 (expect a drop proportional to duplicate content);
- time to first answer on a cold second worktree (target < 60 s vs 17-33 min) and the first-base build time;
- peak RSS of scan and of the daemon with 5 worktrees open (the base block cache is shared once);
- p50 / p95 latency for `references`, `callers`, `implementations`, `search_code` keyword and semantic, `code grep`;
  a regression above 10% p95 on any read mode blocks A8;
- watcher latency when a widely imported file changes (the `restitched` set and the re-stitch time);
- grep bloom skip ratio and candidate counts on 20 fixed patterns before and after A2 (must not worsen);
- promotion / rebase frequency over a week of normal use.

### Open questions for the owner

1. **Base access without a daemon** *(Fjall option; mostly dissolved if the A4 prototype adopts a mmapped sorted-run
   base, which every process can open)*. Fjall admits one process, so with `layered` on every CLI invocation in a group
   contends for the base. The proposal above degrades like a lost lock today (`Daemon` route, else `InRam`). The
   alternative is to make the daemon mandatory for layered workspaces (auto-spawn through `src/comms/singleton_spawn.rs`, and
   fall back to standalone if it cannot start). Which is acceptable for the A8 default?
2. **A2 migration.** Decided: `ROW_VERSION` 2 plus the sweep, no partition-revision bump.
3. **Daemon-less layered mode.** Decided: it degrades like a lost lock today (`Daemon` route, else `InRam`).
4. **Base storage.** Decided: prototype-first. A sorted-run base is built and benchmarked before A4 (*Base storage*);
   Fjall stays for overlays and mutable stores; A4 is not locked to a Fjall base.

## Consequences

Per-worktree disk and scan cost become proportional to the diff, and one block cache serves a whole group.
The read path grows a merge layer that must preserve order, totals and cursors for every mode; the
correctness gate (a proptest over three worktrees with random edits, comparing every mode's results and
cursors to a standalone full index) exists to catch a merge that does not. `store_seed.rs` is removed once
layered mode is the default. Phases A0 (per-repo memory store) and A1 (probe index and O(affected)
re-stitch) shipped in 0.30.0; A2 (`efe41b76`) and A3 (`26be3d56`) shipped on main; A4–A8 remain, specified
below. Until A7, layering reaches only users with embeds off (`documents.embed` defaults to `true`).

Costs accepted by this design: with a Fjall base, a layered workspace depends on the daemon for concurrent base access
(a mmapped sorted-run base would remove that, pending the A4 prototype); the
first base of a group costs one cold build (or one CoW clone when a standalone index exists); migration is
one-way per workspace (rollback re-materialises a standalone index); the document and embedding tiers stay
per worktree until A7; and the overlay is treated as a disposable cache, so any doubt about its consistency
costs a rebuild proportional to the diff rather than a repair.

## Alternatives considered

- **Index only the diffs and query git for the rest.** Rejected: git has no symbols, refs or BM25.
- **Deduplicate full copies.** Rejected: compaction breaks the sharing.
- **One shared Fjall with path-prefixed keys.** Rejected: name scans multiply by the worktree count, and
  one lock covers the whole group.
- **A virtual git-tree filesystem for the resolver, to build the base without exporting.** Rejected: it
  re-implements the resolver's filesystem surface (`intel/resolver.rs`, `resolve_pass.rs`) to save a transient
  1–3 GB export.
- **Eager rebase of idle worktrees when a generation is retired.** Rejected: it needs the worktree on disk
  and its lock; invalidating the overlay and rebuilding it at next open is simpler and costs O(diff).
- **Repairing a suspect overlay in place.** Rejected: the overlay is a cache of the diff; wiping and
  rebuilding it is cheaper to reason about than reconciling `delta.msgpack` against fjall.
- **Opening a Fjall base from every process.** Not possible: fjall's directory lock is exclusive, which is why
  the daemon holds it for a Fjall base (open question 1 covers the no-daemon case). An immutable mmapped base is the
  alternative under prototype (*Base storage*).
