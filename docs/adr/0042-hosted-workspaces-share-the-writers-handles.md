---
status: proposed
date: 2026-11-18
amends: ["0039"]
---

# ADR-0042: Hosted workspaces share the writer's handles; no in-RAM reference fallback

- **Deciders:** the repository owner
- **Related:** amends ADR-0039 (the deletion of the in-process forwarding seams); ADR-0036 (the
  group index and its lock), ADR-0024 (daemon and process model)

## Context

ADR-0039 deletes the in-process forwarding seams (`HostBackend`, `HistoryHost`, `IndexRoute` and
its in-RAM fallback) once processes that do not own the index forward whole tool calls. Two
questions it left open came up when the seams were removed:

- **Where the daemon-hosted workspace reads from.** The hosted read stack opened its store without
  the index (fjall admits one holder per database, even for reading) and reached the daemon's pool
  through `HostBackend` for every index read. The plan sketched the pooled `Workspace` owning a
  writer `Mutex<Store>` beside a read `Store`. The pool, however, sheds the O(corpus) read stack on
  a warm-stack cap and idle clock (`DEFAULT_WARM_READ_STACK_CAP`, `READ_STACK_IDLE_TTL`) while
  keeping the writer store, which holds the index lock and is not free to reopen.
- **What a session holding no index answers.** A CLI command or a second `serve` beside a writer,
  with no daemon, answered `code_refs` call sites, callers and implementations from a byte-budgeted
  projection of the blobs that could be truncated (`projections_capped`). The keyword lane and
  memory already refused in that situation as busy, naming `basemind comms start`.
- **Who serializes git-history builds.** The daemon's broker held a per-repo build lock that only
  calls routed through it took; a hosted session reached it through `HistoryHost`.

## Decision

1. **The writer store stays on the pool entry; the hosted store shares its index handle.** The
   pool entry keeps the writer `Mutex<Store>` and a clone of its `IndexDb`, refreshed when a
   poisoned handle is reopened. The hosted `Workspace` is built over a read-only store whose
   `index_db` is that clone: every read a hosted session makes runs against the index directly and
   never waits on a scan for the writer lock, and the warm-stack cap still sheds the `Workspace`
   alone. A hosted session's scans go to the pool (`WorkspacePool::hosted_rescan`), reached as a
   concrete type from `ServerState`, not through a trait.
2. **A session with no index refuses reference scans.** `code_refs` call sites, callers and
   implementations answer from the session's index view or refuse: as busy (exit `3`) naming
   `basemind comms start` when another process holds the index, or naming `basemind scan` when it
   must be rebuilt. There is no in-RAM call or implementation projection. `code_grep` without an
   index sweeps every file; relation `definition` reads the intra-file resolution facts.
3. **A git-history build serializes on its database handle.** The build lock lives on the
   git-history handle itself and `builder::sync` takes it, so every holder of a clone (the
   broker's socket path, each hosted session) shares one lock. A hosted workspace is built with the
   broker's own handle on the repository's database; the broker's idle sweep keeps a handle a
   hosted workspace still shares, since dropping it from the map would leave its fjall lock held.

## Consequences

- A hosted read is never blocked by a scan of its workspace, and a hosted session's reference,
  search-lane, resolution and memory reads equal a writer's: they read the same handle.
- The daemon socket no longer needs an index-read, resolved-refs, code-search-lane or memory
  request, and the hosted startup git-history sync runs in process on the shared handle.
- A user running the CLI beside a writer `serve` with no daemon gets a refusal for reference scans
  where they used to get an answer that could be silently partial.
- The hosted read store's file map can trail the shared index between a scan's commit and the
  read store's refresh; reads that join the two (a cross-file definition landing in a file) are
  checked against the file's stamp, as before.

## Alternatives considered

- **The pooled `Workspace` owns the writer store**, as the plan sketched. Rejected: shedding the
  read stack would then drop the writer and its index lock, or the cap would have to stop shedding
  the read stack's memory.
- **Keep the in-RAM projection for a session with no index.** Rejected: a second, lossy read path
  that answers a different total and cursor generation than the index, which ADR-0039 already
  names for deletion; the busy refusal names the fix.
- **Keep the build lock on the broker and have hosted sessions call into the broker to sync.**
  Rejected: it keeps a hosted-to-daemon seam for one operation the shared handle can serialize
  itself.
