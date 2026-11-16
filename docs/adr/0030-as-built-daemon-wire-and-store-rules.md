---
status: proposed
date: 2026-11-16
amends: ["0013", "0018", "0020", "0021", "0022", "0024", "0025"]
---

# ADR-0030: As-built rules — daemon generations, verb-tool wire contract, store durability

- **Deciders:** the repository owner
- **Related:** amends ADR-0013, ADR-0018, ADR-0020, ADR-0021, ADR-0022, ADR-0024 and ADR-0025;
  ADR-0027

## Context

Three series landed on `main` after the records they implement were accepted: daemon coexistence
(ADR-0024, ADR-0013), the verb-tool audit (ADR-0018, ADR-0020, ADR-0022) and the storage audit
(ADR-0025). Each made choices those records do not state, and some their text contradicts.
Accepted records are immutable, so this one states the rule now in force for each clause that
changed, checked against `main` at `3d627ba5`. A clause not named here stands.

## Decision

### Protocol generations

1. **ADR-0024 rule 1, "never connect to, probe or stop each other" and "no later binary touches
   that rendezvous", becomes:** no binary connects to, pings, stops or signals another
   generation's daemon, or starts one in the pre-generation rendezvous. It may read other
   generations' registry records (to count the ceiling and name a lock holder), prune one whose
   daemon is provably gone, and take a foreign `daemon.lock` for an instant to test it. The
   registry is generation-scoped (`<data home>/daemons/g<N>/`), so `comms doctor` and `comms stop
   --all` reach their own generation only. Reason: the ceiling must count every generation without
   contacting a foreign daemon.
2. **ADR-0024 rule 7 becomes:** the command is `basemind comms cleanup`. A directory without a
   `daemon.lock` is not a rendezvous; a lock that cannot be opened counts as held; removal renames
   the directory aside under its lock. The pre-generation rendezvous loses every file a release up
   to 0.33 kept in the comms directory.
3. **ADR-0024 rule 1, "each generation runs its own daemon", covers HTTP:** a daemon that finds the
   default port taken binds an OS-assigned one and publishes it in its generation's `http.addr`.

### Takeover and stop

4. **ADR-0024 rule 2 becomes:** "older" and "newer" are semantic-version precedence, pre-release
   included. A newer daemon on another protocol is refused before `Hello`, not asked to stop. An
   older one is asked only when it could be replaced: never under `BASEMIND_NO_AUTOSPAWN`, and by a
   reconnecting relay at most once a minute, so a refusal does not repeat on every reconnect.
5. **ADR-0024 rule 3, "the conditions of `is_idle_for`", becomes:** a daemon refuses `Stop`
   (`daemon_busy`) while it has a relay session or handshake, an inbox subscription, an HTTP
   request, or work of its own, a forwarded CLI `rescan` included; a per-request link does not
   count. A relay link counts from its accept and must send its hello within 30 seconds. A rescan
   forwarded after an accepted `Stop` gets `rescan_draining`. Reason: what refuses is what a stop
   would cut off.
6. **ADR-0024 rule 3, "it does not escalate past a refusal", becomes:** a takeover never does, and
   reports the daemon as busy, failing (`store_error`) or stale (answers `Ping`, not `Stop`). The
   operator's `basemind comms stop` treats `store_error` as silence and may reclaim under rule 7,
   because such a daemon answers only to say its store is broken; `--all` exits non-zero when any
   daemon refused or stayed.
7. **ADR-0024 rule 4 becomes:** a daemon is signalled only after a 12-second exit window and a
   second `Stop` and `Ping` that both went unanswered; a probe that could not be sent establishes
   nothing, and nothing is signalled or spawned. The target is the pid in `daemon.pid`, only when
   `daemon.lock` reads held before and after the pidfile is read, the process is not younger than
   the record (where the platform reports start times) and, in a takeover, it is the pid the daemon
   reported. Reason: a `flock` names no holder; the checks keep a stale pidfile from naming a
   stranger.

### Sessions, relay and launcher

8. **ADR-0024 rule 5 becomes:** CLI `rescan` forwards to a running daemon. A daemon that cannot take
   a workspace declines the relay session with `workspace_locked` (holder named; the session
   retries), `workspace_busy` (its own scan; retried) or `index_newer` (fatal to the session).
9. **ADR-0024's consequence "callers connect per request" becomes:** a `basemind serve` session
   holds one relay link for its life, which keeps the daemon from idling; a daemon exits ten
   minutes after its last session closes.
10. **ADR-0013's relay replay list becomes:** replay safety is decided per tool in
    `src/mcp/replay.rs`, every advertised tool in exactly one list, enforced by a test: read-only
    verb tools are re-sent, `memory_save` and `index_rescan` never, `agents` by mode, `inbox` only
    when it does not mark read. The decision is made when a tool is added.
11. **ADR-0013's "everything else gets a retryable `-32001`", and ADR-0024's consequence that the
    second daemon "serves no index", become:** frames sent with no backend are held 10 s (90 s
    while a connection attempt runs) and forwarded in order if one connects. Past that the relay
    answers `initialize`, `ping` and `tools/list` itself, advertising `tools.listChanged`, and other
    requests with `-32003 backend_unavailable`, retryable unless fatal; on connect it replays
    `initialize` and sends `tools/list_changed`. `-32001` stays for an in-flight request that is not
    replay-safe. Reason: a host gives up a server whose `initialize` fails, and never learns why.
12. **ADR-0024 rule 6 becomes:** the launcher installs under `versions/<version>/`, which no launcher
    up to 0.33 reads, and signals nothing. It prunes a version there or under the old `bin/` only
    when unused for 14 days, run from by no process and not locked by an install; never when the
    process list is unreadable, never through a symlink. ADR-0024's "no new binary can prevent
    that" no longer holds for versions it installs.

### Comms protocol number

13. **ADR-0021 rules 14 and 15 become:** comms `PROTO_VER` 6 carries the batched memory reads and
    `Rescan`'s `verify` flag. Agent comms v2 takes 7 ("Protocol v4" reads v7), as ADR-0027 counts;
    the relay protocol still goes from 1 to 2.

### Verb-tool wire contract

14. **ADR-0022 Contract 1 and ADR-0018's "every list takes `limit` and `cursor`" become:** a cut
    replaces paging where the result is one object or one ranking. `git_diff` stops at 200 hunks or
    128 KiB of text, 200 paths per status list, and skips a file over the size ceiling
    (`too_large`); a `code_read` body stops at 128 KiB on a whole line; `code_outline` imports stop
    at 200, a `git_log {rev}` file list at `max_tokens`; `code_search` and `docs_search` page within
    100 ranked hits. A `memory_search` query takes no `cursor`, a key read no `limit` or `cursor`.
    A `path` ending in `/` is a directory (`git_diff` status, `git_log` commits); without it, error.
15. **ADR-0018's `max_tokens` convention and ADR-0020's `total_is_partial` become one vocabulary:**
    `next_cursor` alone means more; a cut at `limit` is paging; a cut to the token budget sets
    `budgeted`; a cut the caller did not ask for sets `truncated` and `truncated_reason`, with
    `omitted` where known; flags are absent when false. Reasons: `scan_cap`, `byte_budget`,
    `imports`, `size_cap`, `depth`, `rerank_top_k`, `filter_depth`, `path_cap`, `hunk_cap`,
    `byte_cap`, `window`, `shallow_clone`, `live_walk`. Distinct concepts keep their names:
    `skipped_files`, `limit_clamped`, `body_truncated`, `too_large`, `index_status.partial`.
16. **ADR-0018's paging convention adds:** a cursor is bound to its tool and every argument but
    `limit`, `max_tokens` and `cursor`. A foreign cursor, or one that outlived its index generation,
    HEAD or history source, is an error, never an empty page. `limit: 0` is refused.
17. **ADR-0022 Contract 5's empty page extends to `git_log`:** a path or symbol history inspects one
    window of commits per page and may return none with a `next_cursor` (`window`). A name no
    inspected commit matches is listed in `unmatched` and is an error on the last page, unless the
    clone is shallow. A query before the history index is ready searches the newest 10,000 commits.
18. **ADR-0018's "a `notice` reports ... a degraded lane" becomes:** `notice` reports index state
    only (`warming_up`, `building_index`, `rescanning`, `index_rebuilding`, `empty_index`,
    `projections_capped`); a degraded lane is in `lanes_unavailable`; git tools have no `notice`.
19. **ADR-0018's "failures name the fix" covers stale input:** a filter that selects no file is an
    error unless a retry notice says files may be missing. A file edited since the scan is served as
    it is and marked `stale`; where the index cannot answer for it, or it is gone, the error names
    `index_rescan`. No tool loads a file past `scan.max_file_bytes` whole, and `code_grep` follows
    no link out of the repository unless `scan.follow_symlinks` is set.

### Stores

20. **ADR-0025 (a), "a newer version on disk is never wiped", extends to collection:** an older
    binary's GC never marks from, evicts or heals a newer view, and leaves newer blob namespaces and
    newer-view workspaces out of its budget and sweep.
21. **ADR-0025 (a) on blob paths, and "garbage collection must know which schema versions are still
    live", become:** blobs live in `blobs/v<schema>/`; a file map records its `blob_layout`, and one
    of an earlier layout is rebuilt whatever the minor. Older namespaces and pre-namespace blobs are
    reaped past the 6-hour blob grace, by age alone; newer ones are never touched. Reason: blobs are
    derived, and an install still in use re-extracts what it lost.
22. **ADR-0025 (b), "the older binary refuses to open the store at all", becomes:** it serves the
    code index and refuses memory operations only, because the notes are shared by every worktree of
    a group. A writer open that would wipe a view still holding legacy memory rows is refused. Only
    a writer open stamps the version.
23. **ADR-0025 (c) and its vectors row become:** memory-note vectors live per group and embedding
    preset in `repos/<group>/memory.lance/<model>-<dim>-<hash>/`; rows hold no text. Each search
    embeds at most 256 missing notes or 3 seconds' worth, and the next continues.
24. **ADR-0025 (d), "a pending set recorded in the file map", becomes:** the set is the sidecar
    `pending-vectors.msgpack`, synced before the file map that claims its files, so settling a path
    costs one small write. A view records its `vector-store-id`; a lost or replaced store, or an
    unclean shutdown, re-owes every embedded path from blobs.
25. **ADR-0025 (d) extends to blobs,** written without fsync: a leftover scan breadcrumb, or a
    `<data home>/hosting/<pid>` marker (boot id, process start) of a host that died, marks the
    workspace `verify-blobs`, and its next scan is full and validates each file-map and document
    blob it would otherwise trust for existing.

## Consequences

Open, as built:

- CLI `scan` does not forward (ADR-0024 rule 5): with another process holding the writer lock it
  stops with a notice naming it, which ADR-0024's consequences said it must not.
- Rule 7: the lock is not re-checked before SIGKILL, and without start times a process given a dead
  daemon's pid is not ruled out. A forwarded read does not hold off `Stop` (rule 5). A reused pid
  in another generation's stale record counts toward the ceiling until pruned.
- `agents` (8,000 bytes) stands in place of ADR-0022's three agent tools until comms v2 lands.
- The `memory_search` listing sets `truncated` with no `truncated_reason`, outside rule 15. A
  `code_refs` definition answered by the daemon is not one snapshot with the session's store.
- Blob verification skips code-chunk sidecars and `rref` blobs. Owed vector rows settle only while
  a scan runs; vector stores of unused presets are never collected.

Costs: a launcher keeps each version 14 days past its last use; `comms stop` can kill a daemon a
takeover would leave alone; an older install sharing the cache re-extracts blobs older than six
hours.

## Alternatives considered

- **Edit the records in place** — rejected: accepted records are immutable.
- **Supersede ADR-0024 and ADR-0025** — rejected: their decisions stand; these rules refine them.
- **One amending record per series** — rejected: the series meet in the relay, the comms protocol
  number and the memory store, and would state those rules several times.
- **Revert the code to the older text** — rejected per rule: each records a reason the older text
  did not weigh.
