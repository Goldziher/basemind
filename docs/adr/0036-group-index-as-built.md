---
status: proposed
date: 2026-11-17
amends: ["0028"]
---

# ADR-0036: Group index as built — no rollout switch, the group lock, read-only opens

- **Deciders:** the repository owner
- **Related:** amends ADR-0028; ADR-0016, ADR-0025, ADR-0027, ADR-0030

## Context

ADR-0028 is being implemented on one branch that ships with release 0.34.0. That release rescans
every workspace and uses a new blob directory regardless (ADR-0025, ADR-0027). Some of ADR-0028's
rollout rules assume a staged landing across releases, and its text leaves open how its lock is
named and when a read-only process may open the group database. This record states the rules in
force for the clauses that changed. A clause not named here stands.

## Decision

1. **ADR-0028 rule 12, "the work lands behind `[index] group`, default off", becomes:** there is
   no switch. The group index is the only layout from the commit that introduces it, and the
   ADR-0016 code (`store_base*`, `sorted_run*`, `layered_*`, `store_seed.rs`,
   `store_migration.rs`, the shared Lance lane, `BASEMIND_INDEX_LAYERED`, `BASEMIND_NO_SEED`,
   `scan --verify-shared-base`) is deleted before the row families are re-keyed, not with the
   default flip. Of the shared Lance lane only the liveness projection survives, in
   `src/lance/live.rs`. `docs/layered-index-design.md` stays, marked historical, because the
   locked ADR-0016 links to it. Reason: nothing between the stages ships in a release, and a second layout
   doubles every store test for a format that is never released.
2. **ADR-0028 rules 1 and 8, `index.lock`, become:** the group lock is the store's `.lock` with its
   `.lock.meta` holder record, in `cache/groups/<group>/`. Contention reports the holder like any
   other store lock.
3. **ADR-0028 rule 8, "a process that cannot take the lock answers from blobs", is read with
   ADR-0030 rule 26 ("an open without the workspace lock writes nothing"):** a read-only open never
   opens the group database the way a writer does (schema stamp, incarnation, journal flush, wipe).
   It uses a handle its own process already holds. Failing that, a one-shot CLI command run with no
   daemon tries the group lock once, without waiting, as holder "a one-shot basemind query"; when it
   wins, it opens the database for reading only (a database with no incarnation or of another
   schema is not read) and holds the lock until the command exits. A held lock answers from blobs.
   A long-lived reader (`serve` beside a writer) never takes the group lock: every writer of the
   group would wait on it. A writer that meets the query's lock retries as for any holder.
4. **ADR-0028 rule 11, "a workspace's per-view `index.fjall` and `lance/` are deleted when this
   binary first scans it", becomes:** the per-view `index.fjall` is deleted at the writer open of
   each view, before the view is reconciled with the group. The workspace's `lance/` stays until the
   group vector store of ADR-0029 replaces it, since it is still the vector store in use.
5. **ADR-0028 rule 2, BM25 rows, as built first:** postings are shared by every view of the group
   and keyed `term ‖ bm25 key ‖ ordinal`, the 16-byte key covering content hash, language, extract
   epoch and the `[code_search]` chunk digest. Each view keeps its path-keyed `code_bm25_by_path`
   rows for now (rule 6 drops them later): they name the keys the view holds, which its queries
   count and score, give its BM25 totals, and list the terms whose postings a retired chunk leaves.
   A posting is deleted only when no view's forward rows name its key, under a per-group gate that a
   posting commit holds shared and the retirement or sweep exclusively.
6. **ADR-0028 rule 7, order, stands:** name scans order rows by key, offset, path. Each returned page
   is sorted by path, and a cursor encodes the key position.
7. **Rebuilds within the unreleased minor:** the group database records its row layout in group
   `meta`. An open that finds another layout in a database with an incarnation empties every
   keyspace and takes a new incarnation, so every view's file map is emptied and rescanned. A
   read-only open of such a database answers from blobs. `INDEX_PARTITION_REVISION` is not touched.
8. **ADR-0028 rules 2, 3 and 7, symbol and call rows, as built:** `symbols_by_path`,
   `symbols_by_name`, `calls_by_path` and `calls_by_callee` are shared by every view of the group
   and keyed by the 16-byte content key of content hash, language and extract epoch in place of
   the path. Each file map entry records the key its rows were written under (`content_key`), and
   a view reads only through those recorded keys, never recomputed ones: an entry marked for
   extraction again keeps its rows read until the next pass replaces it, and an entry that records
   no key is extracted again. A write of a content reconciles with the rows stored under its key:
   an identical extraction writes nothing, a different one (an extractor change without an epoch
   bump) removes the stored rows the fresh ones lack before writing them, so the two are never
   served together. No scan deletes the rows of a content it stops holding; they wait for the group
   sweep. A view reads through its live keys: the recorded key of each entry of its last written
   file map, with the paths holding it, published each time the map is written, from the map on
   disk when a handle is reopened, and dropped with the view's last handle. A row of a content
   the view does not hold is examined but yields no hit; one held at several paths yields one hit
   per path. The hit key is the row key followed by the path. A `references` or `callers` cursor
   is that hit key behind a digest of the live keys it was cut from (of the in-RAM map's recorded
   keys when the scan is forwarded or runs in RAM), and is refused as stale once they change,
   since an edit moves the edited file's hits in key order. The key is a 16-byte blake3
   truncation: a collision would merge two contents' rows and nothing detects it; at 2^-128 per
   pair this is accepted.

   Until the group sweep lands, every edit leaves a dead content's rows. They count toward a name
   scan's examined-key cap, which stops the page with `truncated` and a cursor at the stop, so
   paging still reaches every live hit, but a hot name in a long-lived daemon needs more pages
   over time; the exact symbol lookup behind `code_search` has no examined cap and walks every
   dead row of its name; and the rows cost disk. The sweep of ADR-0028 rule 10 removes all three.

## Consequences

- Each stage after this one changes the group layout in place; the single
  `INDEX_PARTITION_REVISION` bump of ADR-0028 rule 12 rides the last of them.
- With no daemon, a read-only CLI query answers from the group index when no other process holds
  the group lock, and from blobs while one does (a scan, `serve`, the daemon).
- A writer starting during a one-shot query waits for it within its usual lock retries; a query
  longer than those makes the writer report the query as the holder.
- The per-view index and its tests are gone: the durability tests run against the group layout.

## Alternatives considered

- **Keep the `BASEMIND_INDEX_GROUP` switch until stage 5** — rejected: it kept two layouts and two
  test matrices alive for a format no release carries.
- **Let every read-only open take a free group lock** — rejected for long-lived readers, which
  would lock every writer of the group out; kept for one-shot CLI commands, whose open writes no
  row.
- **Serve read-only CLI queries from blobs only** — rejected: `memory get` and `memory list` were
  refused, and index-backed answers (references, BM25) degraded, whenever no daemon ran.
- **Auto-spawn the daemon for every read-only CLI query** — rejected: a one-shot command would
  start a long-lived process, and `BASEMIND_NO_AUTOSPAWN` would leave the refusal in place.
- **A file named `index.lock`** — rejected: the store lock already records and reports its holder.
