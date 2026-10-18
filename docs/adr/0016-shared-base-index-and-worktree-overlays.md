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
  drifted by 2% or more, or daily; at most three are live, each published once and never written again,
  so the daemon keeps one handle and one block cache per group (fjall admits one process per directory,
  so other processes reach the base through the daemon).
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

Everything below is normative for A2–A8. Paths and symbols are as of 0.30.0 (`bd1f0a5b`). Where the code
contradicts the text of issue #87 or the summary above, the code wins and the correction is listed first.

### Corrections to the issue text

| Claim in #87 | What the code does | Consequence |
|---|---|---|
| "Rev scan builds the base" (`ScanSource::Rev`) | `scan_with_cancel` runs the resolve pass only for `ScanSource::WorkingTree` (`scanner.rs`, `if matches!(source, ScanSource::WorkingTree)`), and the resolver reads the filesystem (`intel/resolve_pass.rs` `std::fs::read`, `intel/resolver.rs` `root.join`, `canonicalize`). A rev scan therefore writes no `refs_by_*`, `import_targets` or `importers_by_probe` rows, and the rev branch of `process_file` writes no grep bloom either. | The base builder exports the committed tree to a private scratch directory and runs a normal working-tree scan on it (A4). A virtual git-tree filesystem for the resolver was rejected: it duplicates the resolver's I/O surface for a transient 1-3 GB export. |
| "Published read-only so the daemon keeps one handle" | Fjall's directory lock is exclusive even for a read-only open (`Store::open_read_only_no_index`, `mcp/index_route.rs` module doc). Read-only is a logical property, not something the OS enforces on a shared fjall. | One process per machine can hold a given base generation. The daemon is that holder; every other process reaches the base through `IndexRoute::Host` / `Daemon` or degrades to `InRam`, exactly as a second `serve` does today (A4). |
| "`code_chunks` is already content-addressed" | `chunk_id` is `<hash_hex>:<ordinal>` (`chunk.rs`) and the sidecar blobs are content-addressed, but the Lance rows are keyed `(scope, path)` (`lance/mod.rs` `replace_code_chunks`, `delete_code_chunks`) and `scope` is the normalised `origin` URL (`git/remote.rs` `scope_key`), identical for every worktree of a clone. | Sharing a Lance table needs a new key for all three tables, not just `documents`/`doc_links` (A7). |
| A3 "shipped" (progress comment) | Only the ADR shipped. `src/index/view.rs` does not exist; `IndexDb` keyspaces are `pub(crate)` and read directly (`mcp/helpers_calls_scan.rs` `calls_by_path`, `mcp/helpers_calls.rs`, `search/bm25.rs` `code_bm25_postings`, `intel/stage_budget.rs`). | A3 is still a code phase and a prerequisite of A5. |
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
  bytes share one row. Row = `version(1) | bits`; `ROW_VERSION` stays 1 because the partition-revision bump
  (below) removes every old row. `build_row(bytes)`; `GrepEntry { rel, hash_hex, size_bytes, mtime_ns }`
  (`index/grep_bloom.rs`, serde, because it crosses the daemon wire).
- **Live-file guard.** `grep_skip_verdicts(root, &[GrepEntry], needle)` skips a file only if the row for
  `entry.hash_hex` rejects the needle *and* `stat(root/rel)` equals the **indexed** `(size_bytes, mtime_ns)` of
  the entry, taken from `FileEntry` (`store.rs`) through `FileMeta` (`mcp/l1_cache.rs`, gains `mtime_ns`). A file
  whose indexed mtime is 0 (git sources) never equals a live stat and stays a candidate.
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
  row behind until that sweep, which is the only deletion path.
- **Schema.** `INDEX_PARTITION_REVISION` 6 to 7 (`index/mod.rs`). Cost: every user re-indexes once (a 17-33 min
  scan on armis) because the bump wipes the whole fjall. A cheaper option exists (ROW_VERSION 2, no bump, the
  sweep deletes the path-keyed rows) and has precedent in the keyspace's own doc comment (added "without a
  schema bump"); the bump is kept because the plan asks for it and it removes any mixed-format window.
- **Wire.** `IndexReadQuery::GrepBloom { pattern, entries }` (`comms/index_read_proto.rs`). A client and daemon of
  different builds fail to decode, and `IndexRoute::grep_skip` already degrades that to "sweep every file".
- **Layered note.** Rows are content-addressed and never shadowed: a layered read does `overlay.get(h)` then
  `base.get(h)`; a stale base row for a dead hash is harmless because nothing asks for it.

### A3: `IndexView` (standalone only)

- `src/index/view.rs`: `enum Part` naming the 16 layered keyspaces (below), and
  `trait IndexView: Send + Sync { get(Part, &[u8]); prefix(Part, &[u8]); range(Part, Bound, Bound); iter(Part); }`
  returning `Box<dyn Iterator<Item = Result<(Slice, Slice), IndexError>> + '_>`, plus the named accessors that exist today
  on `IndexDb`: `references_to`, `definition_of`, `import_targets_of`, `importers_probing`, `is_probed`,
  `import_probes_complete`, `symbols_by_name_lookup`, `symbols_index_is_empty`, `bm25_stats`,
  `grep_bloom_row`, `grep_skip_verdicts`, and the two `NameDict` handles (`callee_names`, `trait_names`).
- `StandaloneView(&IndexDb)` delegates 1:1. `IndexRoute::Local` holds `Box<dyn IndexView>`; `index_read_against`,
  `query.rs`, `mcp/helpers_calls*.rs`, `mcp/helpers_impls.rs`, `search/{bm25,exact}.rs` and the resolve pass
  (`intel/resolve_pass.rs`, which reads `import_targets`, `importers_by_probe`, `refs_*`) take `&dyn IndexView`.
  Writes keep going through `IndexWriter` on the concrete `IndexDb`.
- The existing suite is the gate. Perf discipline applies: the dyn call is per iterator, not per row, but a
  p95 regression above 3% on `references` / `callers` (benches in `benches/`) switches `IndexView` to an enum
  with the two variants instead of a trait object.

### A4: repo-group layout, base generations, migration

**Config.** New `[index]` table in `config/v1.rs` (`IndexConfig`, `deny_unknown_fields`, in `ConfigV1` after
`cache`): `layered: bool`, default `false` until A8. Regenerate `schema/` with the existing
`config_schema` test. The env var `BASEMIND_INDEX_LAYERED=0|1` overrides the file (rollback lever, same pattern as
`BASEMIND_NO_SEED`). The mode is resolved once per `Store::open`; the daemon applies a changed value when it next
opens the workspace. Layered applies to a workspace only when all hold: `layered` is true, the canonical root is the
git workdir, `Repo::discover` succeeds, a base commit resolves, and no embedding lane is enabled until A7 has
shipped (`documents.embed`, `code_search.embed`: their Lance rows are still keyed by worktree).

**Layout.**

```text
cache/repos/<group>/
  memory.fjall/                       A0, untouched
  group.json                          {format, main_root, common_dir, created_unix}
  base.lock                           flock; serialises build / advance / retire / GC for the group
  base/
    gen-<n>/{index.fjall/, index.msgpack, base.json}      published, never rewritten
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
   loop in `IndexDb::open_inner`), close fjall, write `base.json` (tmp + rename inside `.tmp-*`), fsync the directory.
5. `rename(.tmp-*, gen-<n>)` (atomic, same filesystem), then update `CURRENT` (tmp + rename, an advisory hint only).

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

1. Quiesce `k`: if this process is the daemon it takes the write side of the group's `RwLock<Arc<IndexDb>>`,
   drains, and drops its handle; otherwise it tries to open `k`'s fjall and aborts the advance on `Locked`. A live
   fjall is never copied (the rule in the `store_seed.rs` module doc).
2. Clone `gen-k` to `.tmp-*` and reopen the daemon handle immediately.
3. `Repo::diff_trees(C_k, C_m)` (new, gix tree diff; a rename is a delete plus an add) filtered through `Filters`.
4. Export only the added/modified blobs plus the unchanged files the resolve pass probes (A1's `probed_only`
   expansion) to `.tmp-*/checkout/`, apply with `scan_paths_with_observer` through the normal writer, run the
   incremental resolve pass, then steps 4-5 of the cold build.
If the filesystem has no CoW and free space is below twice the generation size, the advance is skipped and retried
later.

**Selection.** An overlay pins `base_generation`. On open without a usable pin (first open, pin retired,
incompatible) choose among compatible ready generations the one with the smallest candidate count (`|K|` below,
computed once per generation, at most three git status calls), ties to the newest. A pin is kept while its
generation exists and is compatible.

**Opening the base.** `IndexDb::open_code_only(gen_dir)`: like `open_tolerating_held_memory` with the memory
store always detached, because the base must not take the group's exclusive `memory.fjall` lock. The daemon's
`workspace_pool.rs` keeps one `Arc<BaseHandle>` per `(group, generation)`; every pooled workspace of the group shares
it and its block cache. A non-daemon process uses `open_index_with_retry`'s budget and then treats `Locked` like a
lost lock today: it routes through `IndexRoute::Daemon` if a daemon answers, else `InRam`. Open question 1.

**Migration of a workspace (standalone to layered), one-way, under the workspace `.lock`.**

1. Resolve or build a base (above). 2. Compute the overlay (A5, "Building the overlay") from the live tree.
2. Verify: for 200 random paths plus every path of `S_path`, compare the merged rows of each layered keyspace with
   the standalone index (`IndexView` makes this one function); any difference aborts and keeps the standalone view.
3. Write `overlay.json` with `state = "ready"`, set `index_mode = "layered"` and `repo_group` in `workspace.json`,
   rename `views/working` to `views/working.migrated`, remove it on the next successful open (so a crash between the
   two leaves a restorable index). `store_seed::seed_working_view` is skipped for layered-eligible workspaces.

### A5: overlay write path and the read merge

**A5.0 (prerequisite, standalone fix).** BM25 postings are refcounted by content hash. `upsert_bm25_file` and
`remove_bm25_file` take a `still_referenced: &dyn Fn(&str /*hash*/) -> bool`, supplied from
`Store.index.files` hash counts, and skip the posting delete when another indexed path holds the hash. A regression
test (two paths, identical content, remove one, the other's postings survive) is red on `main` today.

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

**Merge rule per partition (every keyspace opened by `IndexDb::open_inner`).** Merged order is plain byte order of the
raw key (both layers use the same encoders in `index/keys*.rs`); keys never collide across layers because a base row
is dropped exactly when the overlay may hold a row for the same shadow unit.

| Keyspace | Key (shadow unit) | Overlay holds | Base row dropped when | Notes |
|---|---|---|---|---|
| `meta` | string keys | `delta_seq`, overlay-only `code_bm25_n`, `code_bm25_total_len` | never merged as rows | `schema_ver`: both must equal `INDEX_SCHEMA_VER`. `import_probe_ver`: effective value is the base's (a generation is published only with probes complete); the overlay never writes it. BM25 stats below. |
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
`total_len` likewise, with the subtraction computed from the shadowed base rows at open and on each `S_path` change;
`df(term)` is the count of merged postings rows.

**Gate.** `tests/layered_equivalence.rs`: a proptest over three worktrees of one group with random edit scripts
(modify, add, delete, rename, duplicate content, barrel re-export, `tsconfig` paths change, Python package
`__init__`), each compared against a freshly built standalone index of the same tree for every `Part`, every named
accessor, every MCP mode that reads the index, with totals, `has_more` and cursor round trips. Behind
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
  workspace is never re-layered automatically (no flapping); `BASEMIND_INDEX_LAYERED=1` plus `basemind cache reset
  <workspace>` is the manual way back. Promotion that cannot quiesce the base is deferred and warned in `doctor`.

**Liveness.** A generation is live iff it is the newest ready compatible one or an overlay in the group pins it. At
most three are live; when publishing a fourth, the overlays pinned to the oldest live generation are invalidated (their
overlay dir is deleted, they rebuild on next open). There is no eager rebase of idle worktrees. A non-live generation
is renamed to `.retired-<n>` and removed when its fjall lock can be taken. An incompatible or half-published
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
  table at least 0.95 over 200 queries per tier on armis (`src/eval`); below that, raise `k'` or use exact search for the
  shared table.
- **Doc tier.** Until this phase the overlay carries every document (`Delta.doc_entries`); after it, documents delta
  like code. `scope` stays the normalised remote URL; it no longer separates worktrees.

### A8: layered by default

Preconditions: A5 gate green on 1000 cases, A6 GC merged, A7 shipped, measurement targets met. Then: default
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

### Measurement plan

On armis (89k files, 65k indexed), 1 worktree then 5, standalone vs layered, same machine, cold cache and warm:

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

1. **Base access without a daemon.** Fjall admits one process, so with `layered` on every CLI invocation in a group
   contends for the base. The proposal above degrades like a lost lock today (`Daemon` route, else `InRam`). The
   alternative is to make the daemon mandatory for layered workspaces (auto-spawn through `singleton_spawn.rs`, and
   fall back to standalone if it cannot start). Which is acceptable for the A8 default?
2. **A2 partition-revision bump.** The bump forces one full re-index for every user. The no-bump variant (ROW_VERSION 2 plus the
   sweep) avoids it. The ADR keeps the bump because #87 prescribes it; say if the cheaper variant is preferred.

## Consequences

Per-worktree disk and scan cost become proportional to the diff, and one block cache serves a whole group.
The read path grows a merge layer that must preserve order, totals and cursors for every mode; the
correctness gate (a proptest over three worktrees with random edits, comparing every mode's results and
cursors to a standalone full index) exists to catch a merge that does not. `store_seed.rs` is removed once
layered mode is the default. Phases A0 (per-repo memory store) and A1 (probe index and O(affected)
re-stitch) shipped in 0.30.0; A2–A8 remain, specified below.

Costs accepted by this design: a layered workspace depends on the daemon for concurrent base access; the
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
- **Opening the base from every process.** Not possible: fjall's directory lock is exclusive, which is why
  the daemon holds it (open question 1 covers the no-daemon case).
