---
status: accepted
date: 2026-11-18
amends: ["0028", "0036"]
---

# ADR-0041: Group sweep, BM25 holders, packed postings and group reaping

- **Deciders:** the repository owner
- **Related:** amends ADR-0028 (rules 4 and 10) and ADR-0036 (rules 5, 8 and 9)

## Context

ADR-0036 left three things to "the group sweep": the symbol and call rows of contents no view holds
(rule 9), the posting retirement's read of every view's forward keys whenever a pass deleted a row
(rule 8: about 1.5 s and 75 MiB on a one-file edit of a 4 000-file corpus), and nothing removed
`cache/groups/<group>/` once its last workspace was reaped.

The bench gate of ADR-0028 (`benches/index_read`, at most 10 % p95 regression at three content
versions) failed for BM25 (+142 % hot, +129 % rare in run 44163234234). Two causes, measured:

- Every view rewrote the postings of every content it scanned. Several worktrees of mostly equal
  files left several versions of each posting in the LSM tables, and a term scan merged them all.
- A posting was one row per `(term, chunk)`. A term scan of a group reads the rows of every content
  version and filters them, so its cost grows with rows, and most files are several chunks.

## Decision

1. **ADR-0028 rule 10, "one group sweep, run by the lock holder", as built: it runs at the end of
   every full pass,** in the `group_sweep` lane after the BM25 upkeep, never on a read. It retires
   gone views, then, when the group owes it, deletes the symbol and call rows (with their name
   rows) of every content no view names. An incremental pass never sweeps. There are no `contents`
   markers and no grace period: a content is dead as soon as no file map names it and no writer
   has it pending (rules 3 and 4).
2. **Debt.** Every path write (`upsert_file`, `remove_file`) stages the group `meta` row
   `\xffgroup/content_sweep_owed` in its own batch; a retirement stages it in its synced tombstone
   commit, and a rebuild of a view that held rows sets it before clearing them. A sweep clears the
   token it read, under the group gate, only when no view is unsettled (rule 4), so a debt incurred
   during the sweep survives it. A sweep that kept the debt only because a view was unsettled
   records the token; the next sweep skips the content rows while the token is the same and no
   view settled or was dropped since, so a pass that failed before its file map does not cost
   every later full pass the whole content sweep.
3. **Liveness (amends ADR-0028 rule 10).** A content is live while the sweeping store's file map in
   memory, or any other view's file map on disk (found through the view records), names it under
   the key the entry recorded. A file map that cannot be read and is not provably of another schema
   leaves every content in place and the debt owed; it no longer aborts the sweep, and retirement
   still runs.
4. **Pending contents.** Within the process holding the group lock (no other process writes the
   group), a writer registers a content as pending, and reads the rows stored under it, while
   holding the group gate shared; the sweep deletes in batches, each re-checking the pending set
   with the gate held exclusively. A view is unsettled from its first path write, a removal
   included, until it writes its file map; one written while a sweep runs keeps it unsettled until
   that sweep ends. A view with no open handle is settled.
5. **Gone views (amends ADR-0028 rule 10, "the rows of views whose file map is gone").** A view is
   retired when its view *directory* is gone (the workspace reaper or the cache budget removed it)
   and no handle of this process has it open or claimed. Every open claims its view's id, looking
   it up or allocating it and taking the view's live-key cell, under the view-allocation lock;
   retirement holds the same lock, so the two exclude each other. Under that lock the record is
   replaced by a tombstone in one synced commit that also owes the content sweep, so the id is
   never handed out again; then the view's BM25 rows go (rule 6), every row under its prefix is
   deleted, and the tombstone goes. A sweep first finishes the tombstones a crash left.
6. **BM25 holders (amends ADR-0036 rules 5 and 8; replaces ADR-0028 rule 4 for BM25).** A shared
   keyspace `code_bm25_holders` holds `key ‖ view id ‖ path` for every path of every view holding a
   BM25 key, written in the batch of the forward rows, and only with a forward row. A posting is
   deleted only when no holder row names its key and no writer has the key in flight: a writer
   registers the key under the gate shared before it reads the holders, and releases it after its
   commit. Retirement is one prefix seek per key; the full posting sweep reads the distinct holder
   keys, not every view's forward rows. A view's rebuild or retirement deletes each path's forward
   rows with its holder row in one commit, at most 1 MiB or 4096 posting keys per commit, and
   retires the keys of each commit by point lookup right after it, holding the gate exclusively
   for that batch alone. The full posting sweep is owed while that runs, and stays owed only when a
   forward row does not decode.
7. **Postings are written once per key (ADR-0028 rule 4 for BM25).** A writer whose key another path
   already holds writes only its forward and holder rows. A path writing its own key again (a policy
   reconcile) writes the postings again. A tokenizer change that does not change the key keeps the
   first extraction's postings, as rule 4 accepts for its markers.
8. **Packed postings (amends ADR-0036 rule 5).** `code_bm25_postings` is keyed `term ‖ key`, one row
   per term and content; the value packs `ordinal ‖ tf ‖ doclen` (12 bytes) of every chunk of the
   content holding the term, sorted by ordinal. Document frequency counts entries, scores stay per
   chunk, and hits and scores equal a standalone index (`tests/group_bm25_equivalence.rs`).
9. **Group reaping.** The reaper removes the whole `cache/groups/<group>/` (index and retrieval
   store) of a group no workspace names, under the group lock, by the workspace reaper's
   park-then-delete. `cache/repos/<group>/` (memory notes) stays.
10. **Layout.** `GROUP_FORMAT` becomes 5 (4: holders and once-per-key postings; 5: packed postings).
    A group of another format is emptied and rebuilt (ADR-0036 rule 7).

## Consequences

- After a full pass the group's shared rows equal those of a group built from scratch for its live
  views (`tests/group_bm25_equivalence.rs`). Between full passes the rows of contents dropped by
  incremental passes stay, and count toward a name scan's examined-key cap as ADR-0036 describes.
- A full pass that owes a sweep reads every sibling's file map and seeks once per distinct content of
  `symbols_by_path` and `calls_by_path`. A full pass that owes nothing reads the view records and
  checks each view directory.
- A one-file edit's retirement no longer reads every view's forward keys.
- Writers of every view take the group gate shared once per file; a sweep batch blocks them briefly.
  A view's retirement holds the gate exclusively for one batch at a time. Measured in a debug build
  (`retirement_gate_hold`, 40 terms per file, a second view of as many files kept): retiring a view
  of 4 000 files took 1.36 s with the gate held 1.29 s by the full posting sweep, and now takes
  1.24 s with the longest writer wait 56 ms; at 16 000 files, 9.8 s with a 9.5 s hold before, 4.6 s
  with a 67 ms longest wait now. The read side is unchanged by these rules: `benches/index_read`
  (quick, interleaved before, after, before, after, before, release build) passes the ADR-0028 p95
  gate at three versions for every operation but `bm25/rare` (6.7-7.4 us before, 6.9-8.5 us after;
  the before runs alone spread 11 %, so it is not judgeable), and the writer side indexes the 1x,
  3x and 10x groups in 1.65-1.66, 1.88-1.91 and 2.97-3.11 s against 1.79-1.89, 1.64-1.79 and
  3.03-3.08 s before (the first, cold before run left out: 3.39, 3.18 and 2.83 s).
- A key's postings stay those of its first extraction (rule 7). When a tokenizer change keeps the
  key, a later view's forward rows list other terms, so retiring by those rows deletes only the
  terms it lists; the rest stay until a full posting sweep is owed.
- The posting list holds one row per term and content instead of one per term and chunk.

## Alternatives considered

- **Mark every content written in a `contents` keyspace (ADR-0028 rule 4)** — not needed for symbols
  and calls, whose reconciling write already writes nothing for an identical content; for BM25 the
  holder rows serve as the marker and also answer retirement.
- **Sweep on incremental passes** — rejected: a watcher batch would read every sibling's file map.
- **A pass-length gate instead of pending contents** — rejected: a pass that failed after committing
  rows but before writing its file map would leave contents only its unwritten map names.
- **Retire a view when its file map is missing** — rejected: a view in its first scan has none.
