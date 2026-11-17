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
