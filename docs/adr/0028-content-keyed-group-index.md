---
status: accepted
date: 2026-11-17
supersedes: "0016"
amends: ["0024", "0025"]
---

# ADR-0028: One content-keyed index per repository group

- **Deciders:** the repository owner
- **Related:** supersedes ADR-0016; amends ADR-0024 (rule 5) and ADR-0025 (clause (d) and the
  index and vector rows of its table); ADR-0012, ADR-0023, ADR-0027, ADR-0029

## Context

Each git worktree is its own workspace, keyed by the blake3 of its canonical root
(`src/store_layout.rs`). Every worktree, and every `staged` or `rev-*` view of it, owns a full Fjall
index and a full vector store. Only blobs are shared.

ADR-0016 answered with an immutable base index plus a per-worktree overlay. About 5,200 lines of it
exist and it does not apply in practice: it gives up above 32 candidate files and whenever
documents or code search are enabled (`eligible` in `src/store_base.rs`), which is the default.

Measured on one machine: 25 worktrees of one repository hold 28,058 path entries and 1,349 distinct
file contents; an indexed worktree is 20 MB of journal against 1.9 MB of rows. On ADR-0016's
reference monorepo the Fjall index is 2.6 to 2.8 GB per worktree.

Facts checked in the code and in fjall 3.1.13:

- **The journal cannot be shrunk.** Fjall preallocates a 64 MiB journal, seals it only at a
  memtable flush after it passes 64 MB, and refuses a cap below 64 MiB. The cost is per database,
  so the remedy is fewer databases.
- **What a file contributes depends on more than its bytes.** Symbols, calls and implementations
  follow from content, language and extract epoch. Code chunks and BM25 postings also follow from
  the `[code_search]` chunk settings. Only cross-file resolution depends on the tree.
- **Fjall admits one process per directory, for reads too.** A "read-only" open takes the same
  lock.
- **Blobs are keyed by content hash alone,** inside a directory per blob schema version.

## Decision

One Fjall index per repository group, keyed by content. A worktree costs its file map and its
cross-file rows.

1. **Group.** The group key is `repo_group_key`: the blake3 of the canonical main worktree root; a
   root outside git is a group of one. Derived group state lives in `cache/groups/<group>/`:
   `index.fjall`, `index.lock`, `group.json` and `lance/` (ADR-0029). `cache/repos/<group>/` keeps
   the memory notes and their vectors and stays outside every collector. The registry's `RepoId`
   is not the key: it differs between worktrees of a clone without a remote.
2. **Row keys.** A row is keyed by a 16-byte blake3 of the content hash and everything else that
   determines the row:

   | Rows | Key | Derived from |
   |---|---|---|
   | symbols, calls, implementations, intra-file edges | `ck` | content hash, language, extract epoch |
   | BM25 postings, code-chunk vectors | `cck` | the `ck` inputs and the code chunk digest |
   | document chunks (ADR-0029) | `doc_key` | content hash and document extraction digest |
   | grep bloom | content hash | unchanged |

   A posting key becomes `term ‖ cck ‖ ordinal`, 18 bytes in place of a 67-byte chunk id.
3. **File map.** It stays at `workspaces/<ws>/views/<view>/index.msgpack` and gains the group
   index incarnation, the view id, the view's seal generation, its BM25 totals, and per entry the
   keys its rows were written under. Liveness and collection read keys from file maps. They never
   recompute them from configuration, which may have changed or be gone.
4. **Content rows.** A `contents` keyspace holds one marker per key and row family, committed in
   the batch that completes that family's rows. A scan that finds the marker writes only the file
   map entry.
5. **Cross-file rows.** Cross-file reference edges, `import_targets` and `importers_by_probe` keep
   path keys under a 4-byte view id. A view id is allocated with a synced commit, is never
   reused, and its record names the workspace and view.
6. **Dropped keyspaces.** `imports_by_module`, `imports_by_path` and `embeddings` have no reader.
   `symbols_by_path`, `implementations_by_path` and `code_bm25_by_path` served per-path deletes,
   which content rows do not need; the "index is empty" test moves to `contents`.
7. **Reads.** `IndexView` takes and returns paths. Its one implementation filters content rows
   through an in-memory map of the view's live keys and expands a row to every live path holding
   that content. Name scans order hits by name, key, offset, path; the cursor is the row key plus
   the path. BM25 uses the view's own document count and total length and counts only live
   postings, so scores equal those of a standalone index.
8. **Lock.** `index.lock` is the group lock. Only its holder opens `index.fjall` or writes the
   group's vector store, and it keeps one handle shared by every pooled workspace of the group.
   A process that cannot take the lock answers from blobs and does not open the database at all.
   ADR-0024 rule 5 becomes "one writer per group index, enforced by the group lock".
9. **Durability.** Each view has its own seal generation in the group index. A scan writes rows,
   commits the view's next generation with a sync, then writes the file map carrying it. A file map
   ahead of its view's seal, naming another incarnation, or carrying a view id the index does not
   hold for it, is emptied and its view rows are cleared. The group index and every other file map
   stay. This replaces "discarded together with that index" in ADR-0025 clause (d).
10. **Collection.** One group sweep, run by the lock holder under a per-group gate that excludes
    that group's scans, takes the union of keys named by every file map of the group, found
    through the view records. It deletes the markers of keys dead for longer than the blob grace
    period, then every content row whose key has no marker, then the rows of views whose file map
    is gone, and it hands the live document and chunk keys to the vector store (ADR-0029). An
    unreadable file map that is not provably of another schema aborts the sweep.
11. **Migration.** Nothing is converted. A workspace's per-view `index.fjall` and `lance/` are
    deleted when this binary first scans it; where no such scan happens, the orphan reaper removes
    them once their lock has been free and they have been unused for 30 days. An older binary
    refuses a migrated workspace by its file map, which stays at the same path.
12. **Rollout.** The work lands behind `[index] group`, default off, in this order: live-key map
    and row keys; the group database and lock; then the re-keying of row families and ADR-0029's
    document store, side by side; the group sweep; the default. `INDEX_PARTITION_REVISION` is
    bumped once, when the default flips, and the ADR-0016 code is deleted in that change. It is the
    only format bump of both records besides the 0.34.0 minor itself; no stage before it ships a
    format in a release.

## Consequences

- A worktree costs its file map plus cross-file rows: about 0.2 MB against 22 MB on the measured
  repository. One journal per group replaces one per worktree and view.
- **Upgrade.** Release 0.34.0 rescans every workspace regardless (ADR-0025, ADR-0027) and uses a
  new blob directory, so each distinct content is extracted and embedded once per machine. After
  that a new worktree costs hashing its files.
- **Result order changes.** `references`, `callers` and `implementations` list files in key order
  within a name. Cursors from an older binary are invalid.
- **Cursors are weaker across a rescan.** Unchanged files are never repeated or skipped. A file
  edited between two pages gets a new key and can appear twice or not at all in that listing.
- A name scan reads the rows of every content version in the group, then filters. Rows of other
  worktrees count toward the examined-key cap, so a scan can stop with `total_is_partial` and a
  cursor where it completed before. A p95 regression above 10% at three versions per content
  blocks the default.
- Scans of two worktrees of a group share one journal writer. Concurrent scan throughput is
  measured before the default flips.
- With no daemon, a second process in another worktree of the group answers from blobs; today it
  opens its own index. Two basemind generations cannot serve one group at once.
- A newer binary that has scanned one worktree locks the older one out of the whole group.
- Dead rows cost disk and filtered reads until the sweep runs. The live-key map costs memory per
  pooled view.
- A moved main checkout re-keys the group. The index is rebuilt from blobs; memory notes are
  stranded, as today.
- About 5,200 lines are deleted: `store_base*`, `sorted_run*`, `layered_*`, `store_seed.rs`,
  `store_migration.rs`, `lance/shared*.rs` and `docs/layered-index-design.md`. Of the shared
  Lance lane only the widening projection survives, moved for ADR-0029.

## Alternatives considered

- **Finish base plus overlay (ADR-0016)** — rejected: one base cannot be near 25 worktrees that
  each differ by hundreds of files; the overlay needs a merge layer that preserves order, totals
  and cursors for every keyspace; and it excludes the default configuration.
- **One group-wide seal generation** — rejected: after a lost journal tail the next seal of any
  view re-issues the lost number, and a file map written before the loss then passes the check
  while naming contents whose rows are gone.
- **Key every row by content hash, language and epoch only** — rejected: two worktrees with
  different chunk settings would overwrite each other's postings under one key.
- **Let processes without the lock open the index read-only** — rejected: the open is exclusive,
  so a reader in one worktree would lock the writer out of every other.
- **Copy-on-write seeding of a sibling's index** — rejected: compaction rewrites the pages, so
  clones become full copies, each with its own journal.
- **One shared Fjall with path keys under a view prefix** — an intermediate step only: it removes
  the journals but stores every row once per worktree.
- **Group identity from the remote URL** — rejected: it splits a clone without a remote and merges
  two clones of one remote under a single lock.
- **Keep per-worktree indexes** — rejected: disk and first-scan cost grow with the worktree count.
