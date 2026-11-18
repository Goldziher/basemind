---
status: accepted
date: 2026-11-18
amends: ["0025", "0030"]
---

# ADR-0033: Store rules after the sixth storage review — evidence, collection, recovery

- **Deciders:** the repository owner
- **Related:** amends ADR-0025 and ADR-0030; ADR-0024, ADR-0028

## Context

The sixth storage review of `fix/storage-round-6` found that ADR-0030's store rules no longer
describe the code, and that some of the code broke ADR-0025 (a). `basemind watch` writes a hosting
marker though rule 25's consequence says only the daemon does; a Linux full pass syncs before it
drops its breadcrumb; an undecodable file map of a newer blob layout was discarded as damaged; the
garbage collector, the git-history index and the code index each gained a recovery rule no record
states. Accepted records are immutable, so this one states each rule now in force. A clause not
named here stands.

## Decision

### Evidence of unsynced blobs

1. **ADR-0030 rule 25, and its consequence "Only a daemon writes a hosting marker", become:** a
   `<data home>/hosting/<pid>` marker is held by the daemon, by `basemind watch`, and by every
   incremental pass (`scan_paths`, a watcher batch, `basemind rescan <paths>`, `index_rescan` with
   paths) for its duration. A process holds one marker however many hold it at once; the last to
   let go syncs the blob filesystem and then removes it. `basemind serve` has no standalone mode
   (it relays to the daemon), so it is covered by the daemon's marker.
2. **A full pass** removes its breadcrumb after a sync of the blob filesystem (`syncfs`), unless
   the process holds a hosting marker, whose removal syncs instead; so no sync runs while a host
   holds the store's write lock. The sync is Linux-only: on macOS and Windows a finished pass, and
   a released marker, leave no evidence, and their blobs are trusted for existing after a power
   loss.

### Collection

3. **ADR-0030 rule 20 becomes:** the collector heals nothing. A view of an older schema or blob
   layout, or one whose map does not decode, is passed over without the workspace lock and never
   removed by the blob sweep. Budget eviction never evicts a view whose map reads as a newer
   schema or blob layout, or whose version cannot be read.
4. **ADR-0030 rule 21 adds:** a file map of a newer `blob_layout` is refused as newer by every
   open and by eviction, read from the map's version fields before the rest is decoded.
5. **ADR-0030 rule 22 extends to collection:** before eviction or the orphan reaper removes a
   workspace, every view whose index still holds the pre-0.30 notes keyspace has its notes moved by
   a writer open under the workspace lock, judged by the index itself, not by the version of its
   file map. The notes go to the group `workspace.json` records (rule 14), or, when it records
   none, the group of a root that still exists. A workspace whose notes cannot be moved is kept,
   reported as `legacy_memory` (eviction) or `kept_unverifiable` (reaper).
6. **The orphan reaper** takes a workspace root for deleted only on a definite "not found". A root
   it cannot check (a permission error, a stale mount) is kept as `kept_unverifiable`, as is one
   whose parent cannot be confirmed.
7. **`telemetry.jsonl`** is renamed to `telemetry.jsonl.1` at 8 MiB, replacing the previous
   generation, so one workspace keeps at most two.

### Recovery

8. **ADR-0025's file-map row, "rebuild / refused", becomes:** a map whose version fields read as
   this binary's schema and a blob layout no newer than its own, but which does not decode, is
   rebuilt by a writer open. A map whose version fields cannot be read is renamed aside to
   `index.msgpack.unreadable-<time>` by a writer open, never deleted, and the view is rebuilt. A
   read-only open refuses both, so only `basemind scan` or the daemon heals them.
9. **ADR-0030 rule 26 adds:** a scan into a store opened without the workspace lock takes the lock
   first and keeps it, or is refused while another process holds it (an `isError` result naming
   the holder over MCP, exit `3` from the CLI). Having taken it, the scan opens the code index as a
   writer does (stamp, notes migration, sweep of discarded directories) instead of writing through
   the handle the read-only open took; a handle another reader still holds is kept.
10. **ADR-0025's git-history row adds:** a build or an append marks the index in progress and
    removes its head in one batch, before its first row; the batch that stamps the new head clears
    the mark. A marked index has no head and is rebuilt. A build or append that fails records the
    HEAD it was for; for an hour the next sync at that HEAD leaves the marked index alone (tools use
    the live walk) instead of rebuilding the whole history only to fail again. A clear empties the
    metadata keyspace and syncs the journal before it empties the others, so a power loss cannot
    bring the head back over emptied postings.
11. **The code index records whether a pass that writes rows is open** (`rows_unlisted` in `meta`).
    A pass sets it before its first row. Only the flush that ends a pass which was not cancelled
    clears it, in the same synced batch as that flush's seal; no other seal clears it. A pass that
    finds it set, or finds no record at all (an index from before the record), removes the rows of
    every path no file map lists: a full pass after its drive, among the paths its drive did not
    see, and an incremental pass before its drive. Removals are committed a few hundred paths at a
    time. An in-place rebuild of the code index empties its metadata keyspace and syncs the journal
    before it empties the rest.
12. **ADR-0030 rule 26 extends to the vector store:** only a process holding the workspace lock
    creates, wipes or rebuilds `lance/`. A search in any other process (a read-only serve, the
    one-shot CLI) opens it read-only: a store that is absent, built for another embedding model or
    dimension, or written by a newer basemind is refused with that reason, and the lane reports
    itself unavailable. Before, such a search wiped the writer's tables, and a writer and a reader
    of different presets wiped each other on every search.
13. **ADR-0030 rule 24 adds:** a vector debt whose cached blob is damaged (it exists but does not
    decode) is not settled. It stays owed, and the file is sent to be extracted again (a code
    file's entry gets extraction epoch 0; a document's entry is dropped), so the next pass that
    reaches it re-chunks it and repays the debt.

### Groups

14. **`workspace.json` records the workspace's repository group** (`group`), written when the
    workspace is first opened by this version. Collection reads it to place notes of a workspace
    whose root is gone.
15. **`basemind cache stats` reports the per-repository group directories** (`repos/`): their total
    size and the number no workspace marker names. Report only: nothing under `repos/` is evicted,
    reaped, or counted against the cache budget.

## Consequences

- A view of a newer layout, or one whose map's version is unreadable, now costs disk until a writer
  of its own version or a cache clear removes it; an unreadable map set aside is never collected.
- A one-shot incremental pass on Linux pays one `syncfs` at its end.
- A deterministic git-history failure leaves a repository on the live walk for up to an hour per
  HEAD.
- The first pass on an index from before rule 11 walks every path of the index once.
- Open: a group directory whose clone is gone is never reaped, and one that no marker names cannot
  be told from one whose markers predate rule 14. A marker in the group directory naming its main
  root would let a reaper decide; nothing writes one yet.
- Open: blobs on macOS and Windows have no sync before their evidence is dropped (rule 2).

## Alternatives considered

- **Edit ADR-0030 in place** — rejected: accepted records are immutable.
- **Delete a map whose version cannot be read** — rejected: it may be a newer basemind's, which
  ADR-0025 (a) forbids wiping; renaming it aside keeps the workspace openable without losing it.
- **Roll a failed git-history append back by ordinal** — rejected for now: the postings it rewrote
  would each need truncating, which costs a walk of every posting list; the backoff bounds the cost
  of the rebuild instead.
- **Clear the code index's pass record in every seal** — rejected: a seal that writes no rows (the
  rewrite of an emptied file map, the claim of a store opened read-only) then hid an interrupted
  pass, which is the defect rule 11 exists to close.
