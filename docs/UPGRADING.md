# Upgrading an existing index

basemind keeps its index in a machine-global cache (`BASEMIND_DATA_HOME`, default
`~/.local/share/basemind`): per-workspace views, LanceDB tables and git history under
`cache/workspaces/<key>/`, and one content-addressed blob store under `cache/blobs/` shared by every
workspace. An upgrade never asks you to wipe it. This page says what happens to the old shape and what
is cleaned up.

## What changes on upgrade, and how it migrates

| Change | Mechanism | Cost |
| --- | --- | --- |
| Prose, data and config files (markdown, json, yaml, toml, xml, csv, ini, ...) left the code map for the document tier | The next scan, full or incremental, notices a path indexed in the other tier, purges its code-map rows (file map, fjall `files` / calls / implementations / resolved / bm25 entries, code-search chunks) and indexes it as a document | Documents are chunked, and embedded when `[documents] embed` is on |
| Symbol `signature` now ends at the grammar's body field instead of embedding the body | `EXTRACT_EPOCH` (see below): only files whose entry or blob predates the epoch are re-extracted, in the normal bounded scan | One parse per affected file, no wipe |
| Release-minor schema bump (`0.27.x` to `0.28.x`) | `RELEASE_MINOR` mismatch: each view's `index.msgpack` and fjall index are reset on open and every file is re-extracted, overwriting stale blobs in place | Full re-extraction, by design |
| `grep_bloom` index keyspace (per-file trigram bloom for `code grep`) | A new keyspace, so `INDEX_SCHEMA_VER` is not bumped. An index without it opens with an empty `grep_bloom` and greps correctly at once (every file is a candidate); the next scan builds the rows from the bytes it reads, and an unchanged file with no row is read once to backfill it | One read per unchanged file, once; about 12.8 % of the indexed code bytes on disk |
| Resident term index, name dictionaries, heap retag | In memory only. Nothing on disk, nothing to migrate | none |
| `[resources] onnx_provider` defaults to `cpu` | Behaviour only: models load on the CPU provider instead of the platform default (CoreML on macOS). Set `onnx_provider = "auto"` to restore it. No on-disk change | none |
| Daemon `IndexRead` forwarding | Wire protocol only | none |

## The extractor epoch

`SCHEMA_VER` equals the release minor, and a mismatch wipes every cache, so a patch release can never
use it. When an extractor fix changes what an unchanged file's blob should contain but not how it is
serialized, bump `extract::EXTRACT_EPOCH` instead:

- every `FileEntry` and every L1 blob records the epoch it was produced under (`0` when written before
  the field existed, so older data deserializes unchanged);
- the unchanged-file shortcut and the blob-reuse path both require `epoch >= EXTRACT_EPOCH`, so a
  stale file falls through to a fresh parse and its blob is rewritten in place;
- the rewrite keeps the L2 call tier of the old blob, and runs inside the same memory-bounded scan as
  any other changed file;
- the scan reports it as `refreshed` (and `tier_migrated` for tier moves).

Epoch history: `1` is header-only signatures. A binary that predates the epoch can still read the data
(extra fields are ignored); it treats entries as epoch `0`, so the next new-build scan refreshes them
once more.

## Automatic cleanup

Content-addressed blobs are shared and never rewritten on their own, so a migration orphans some of
them: the `.fm` / `.chunk` / `.rref` blobs of a path that became a document keep the same content hash
as its live `.doc` blob.

- The sweep is tier-aware. A `.doc` blob is live only if some workspace's `doc_files` references the
  hash; `.fm`, `.chunk` and `.rref` blobs are live only if a `files` entry does. Before this, the
  shared stem kept the dead code-lane blobs alive forever.
- It reference-counts against every workspace on the machine, keeps blobs younger than 6 hours (a
  concurrent scan may be about to reference them; `BASEMIND_BLOB_GC_GRACE_SECS` overrides), and takes a
  machine-wide `cache/gc.lock` so two sweeps never build the live set at once.
- A scan that migrated a path, refreshed an extraction, or reset a view for a schema bump runs the
  sweep itself and prints `cleanup: reclaimed N orphaned blob(s), B bytes`. In the daemon the same
  condition triggers a sweep one minute later. The hourly daemon sweep and `basemind cache gc` run it
  too; `cache gc` used to be a report-only no-op.
- The result is recorded in `gc-state.json` and shown as `last_gc` by `cache stats` / `admin
  cache_stats`; `orphan_blob_count` there is tier-aware and drops to `0` after the sweep.

A second scan after the upgrade is a no-op: nothing is migrated, refreshed or reclaimed, and the blob
set stops changing.

## Clearing and downgrading

`basemind cache clear --component blobs|views|lance|git-cache|telemetry|all` covers every on-disk
component the migration touches; the term index and name dictionaries are memory only. The sweep lock
(`cache/gc.lock`) is an empty file and is removed with the cache root. `git-history.fjall` is only
removed by `all`: the daemon holds it open.

Opening upgraded data with an older binary fails safe. The index extras are ignored, a binary on the same
schema minor re-maps the files it still treats as code (and drops their document entries), and one on a
different minor resets the view and re-extracts, as for any schema change. Going back and forth costs
re-extraction, never correctness; the next sweep reclaims what is left.

## Per-version binaries

The plugin launcher keeps one `~/.cache/basemind/bin/<version>/` per release, reaps processes of other
versions and removes older version directories on every launch of the current one. That cache is a few
tens of MB per version and is not touched by the above.

## Adding a new on-disk component (checklist)

A derived index should plug into the same hooks so it upgrades and cleans up like the rest (the grep
trigram bloom, `src/index/grep_bloom.rs`, is a worked example: a versioned row per file, a missing or
older row reads as "no information" and is backfilled by the next scan):

1. Stamp the component with its own format version and treat a mismatch as "rebuild lazily", never as an
   error. If it is per-file, store the stamp on the `FileEntry` (or the blob) the way `extract_epoch`
   does, so only stale files are rebuilt, inside the bounded scan.
2. If it adds a blob suffix, add it to `BLOB_SUFFIXES` and, when a code-map entry owns it, to
   `CODE_LANE_SUFFIXES` (`src/store_gc_live.rs`); a doc-tier suffix needs no entry. A suffix in neither
   list is never swept.
3. If it writes under a workspace, add it to `CacheComponent` / `clear_component_in` and to
   `cache_stats` so `cache clear` and `cache stats` account for it.
4. Count anything the pass displaced in `ScanStats` and fold it into `displaced_artifacts()`; the CLI
   scan and the daemon then sweep promptly.
5. Add a case to `tests/upgrade_from_release.rs` (it skips when no released binary is cached) and a unit
   test next to the new component.
