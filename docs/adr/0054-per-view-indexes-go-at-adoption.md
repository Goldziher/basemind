---
status: proposed
date: 2026-11-20
amends: ["0028"]
---

# ADR-0054: Per-view indexes go when the group index adopts their view

- **Deciders:** the repository owner
- **Related:** amends ADR-0028 (rule 11); ADR-0036 (the group lock), ADR-0041 (group reaping)

## Context

ADR-0028 rule 11 deletes a workspace's per-view `index.fjall` when this binary first scans it, and
leaves the rest to the orphan reaper, "once their lock has been free and they have been unused for
30 days". No code implements the 30-day rule, and the orphan reaper removes only workspaces whose
root is gone. A worktree nobody opens after the upgrade keeps its old index for good: about 2.9 GB
each on a repository of about 100,000 files, for every such worktree.

A release up to 0.33 reads those indexes, and one may still be running: its daemon keeps serving
through an upgrade, and the plugin launcher keeps an unused version installed for 14 days. The blob
sweep has the same problem for the blob namespaces of older releases, and checks for it by the
daemons (`daemons/`, every generation) and hosts (`hosting/<pid>`) registered under the data home
(`Holds::live` in `src/store_gc_namespaces.rs`).

## Decision

1. **ADR-0028 rule 11, the clause after "when this binary first scans it", becomes:** the group
   adopts every view of a workspace once the group index of the workspace's repository exists
   (`cache/groups/<group>/index.fjall`). The daemon's cache sweep then deletes the per-view
   `index.fjall` of each of those views, whether or not this binary has opened the worktree
   (`src/store_gc_legacy_views.rs`). The 30-day rule is dropped.
2. **Nothing is deleted while a release that reads per-view indexes runs.** Every such release reads
   the un-namespaced blobs, so while `Holds::live` names that namespace the sweep deletes none.
   There is no hold after the last such process exits.
3. **The sweep takes the workspace lock without waiting** and skips a workspace whose lock another
   process holds. Under the lock it first moves the memory notes a release before 0.30 kept in a
   view's index to the group's notes store (`rescue_legacy_memory`); a workspace whose notes cannot
   be moved keeps its indexes. The file map (`index.msgpack`) stays, as at a writer open.

## Consequences

- One sweep after the upgrade frees the old indexes of every adopted worktree, opened or not; the
  sweep's report counts them (`legacy_indexes_reaped`, `legacy_index_bytes_freed`).
- A release up to 0.33 started after the sweep finds the index of an adopted worktree gone.
- A workspace whose repository has no group index (no worktree of it opened since the upgrade, or
  its group evicted) keeps its per-view indexes.
- Rule 11's handling of the workspace's `lance/` is unchanged.

## Alternatives considered

- **The 30-day rule of ADR-0028.** It keeps several gigabytes per worktree for a month after the
  upgrade, to protect a process the live check already protects.
- **Delete without a live check.** A 0.33 daemon still serving a worktree from its index would lose
  it under a running query.
