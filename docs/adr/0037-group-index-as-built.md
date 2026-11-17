---
status: proposed
date: 2026-11-17
amends: ["0028"]
---

# ADR-0037: Group index as built — no rollout switch, the group lock, read-only opens

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
   ADR-0030 rule 26 ("an open without the workspace lock writes nothing"):** opening the group
   database writes (journal, schema stamp, incarnation), so a read-only open uses the group index
   only through a handle its own process already holds, and otherwise answers from blobs. It never
   takes the group lock. A writer open takes it.
4. **ADR-0028 rule 11, "a workspace's per-view `index.fjall` and `lance/` are deleted when this
   binary first scans it", becomes:** the per-view `index.fjall` is deleted at the writer open of
   each view, before the view is reconciled with the group. The workspace's `lance/` stays until the
   group vector store of ADR-0029 replaces it, since it is still the vector store in use.

## Consequences

- Each stage after this one changes the group layout in place; the single
  `INDEX_PARTITION_REVISION` bump of ADR-0028 rule 12 rides the last of them.
- With no daemon, every read-only CLI query answers from blobs, in the main worktree too, until a
  writer in the same process holds the group.
- The per-view index and its tests are gone: the durability tests run against the group layout.

## Alternatives considered

- **Keep the `BASEMIND_INDEX_GROUP` switch until stage 5** — rejected: it kept two layouts and two
  test matrices alive for a format no release carries.
- **Let a read-only open take a free group lock** — rejected: the open writes, which ADR-0030 rule
  26 forbids without the lock a writer holds, and a long-lived reader would lock every writer of
  the group out.
- **A file named `index.lock`** — rejected: the store lock already records and reports its holder.
