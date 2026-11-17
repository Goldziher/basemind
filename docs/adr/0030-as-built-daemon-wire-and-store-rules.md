---
status: accepted
date: 2026-11-17
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
   --all` reach their own generation only; so is the comms directory, `daemon.log` included. The
   number is `RENDEZVOUS_GENERATION` in `src/daemon_lock.rs`. Reason: the ceiling must count every
   generation without contacting a foreign daemon.
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
   request, or work of its own (blob GC, a git-history sync, a forwarded CLI `rescan`). A
   per-request link does not count, so a read or memory operation forwarded over one does not hold
   off `Stop`. A relay link counts from its accept and must send its hello within 30 seconds. A
   rescan forwarded after an accepted `Stop` gets `rescan_draining`. Reason: what refuses is what a
   stop would cut off.
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
   reported. In a takeover a daemon asked to stop has 3 s to stop answering `Ping`; SIGTERM is
   followed by SIGKILL after 5 s in a takeover and 3 s under `comms stop`, then 2 s are allowed for
   the exit; Windows has no SIGTERM and goes straight to `taskkill /F`. With the lock free after the
   window, the daemon is gone when nothing accepts on its socket and `Unidentified`, signalled by
   nobody, when something does. On Windows a pid a record names whose process refuses `OpenProcess`
   (access denied) gives no start time to rule it out, and is read two ways. Where a wrong reading
   costs a delay or a retry (naming or signalling a holder, waiting for a lock record, counting
   other generations' daemons, a scan breadcrumb, a parked directory) it is not live: basemind
   processes of one account open each other. Where the record would be deleted (a hosting marker,
   this generation's registry pidfile) it may be live and the record stays: a basemind of another
   account or a sandboxed one sharing the data home refuses too, and losing its record hides a live
   daemon from `comms doctor`, `stop --all` and the ceiling, or a host that later dies with the
   machine from blob verification. Reason: a `flock` names no holder; the checks keep a stale
   pidfile from naming a stranger.

### Sessions, relay and launcher

8. **ADR-0024 rule 5 becomes:** CLI `rescan` forwards to a running daemon. A daemon that cannot take
   a workspace declines the relay session with `workspace_locked` (holder named; the session
   retries), `workspace_busy` (its own scan; retried) or `index_newer` (fatal to the session).
9. **ADR-0024's consequence "callers connect per request" becomes:** a `basemind serve` session
   holds one relay link for its life, which keeps the daemon from idling; a daemon exits ten
   minutes after its last session closes.
10. **ADR-0013's relay replay list becomes:** replay safety is decided per tool in
    `src/mcp/replay.rs`, every advertised tool in exactly one list, enforced by a test: read-only
    verb tools are re-sent, `memory_save` and `index_rescan` never, `agents` by mode (decided in
    `src/stdio_relay.rs`): its reads, `ack` and `wait` are re-sent, `post` too, with a
    relay-generated `idempotency_key` when the caller gave none so the replay cannot store a second
    copy, and `inbox` only when it does not mark read. The decision is made when a tool is added.
11. **ADR-0013's "everything else gets a retryable `-32001`", and ADR-0024's consequence that the
    second daemon "serves no index", become:** frames sent with no backend are held 10 s (90 s
    while a connection attempt runs) and forwarded in order if one connects. Past that the relay
    answers `initialize`, `ping` and `tools/list` itself, advertising `tools.listChanged`, and other
    requests with `-32003 backend_unavailable`, retryable unless fatal; on connect it replays
    `initialize` and sends `tools/list_changed`. `-32001` stays for an in-flight request that is not
    replay-safe. A request the backend has not answered in 180 s gets `-32002`
    (`BASEMIND_RELAY_REQUEST_TIMEOUT_SECS`, `0` for none); `initialize` is exempt. Reason: a host
    gives up a server whose `initialize` fails, and never learns why.
12. **ADR-0024 rule 6 becomes:** the launcher installs under `versions/<version>/`, which no
    launcher up to 0.33 reads, and signals nothing. It prunes a version there or under the old
    `bin/` only when unused for 14 days, run from by no process and not locked by an install; never
    when the process list is unreadable, never through a symlink. ADR-0024's "no new binary can
    prevent that" no longer holds for versions it installs.

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
    100 ranked hits, and keep a query's ranked list for 2 minutes (16 lists, 4 MiB; a rescan drops
    them) so a later page does not rank again. A `memory_search` query takes no `cursor`, a key read
    no `limit` or `cursor`. A `path` ending in `/` is a directory (`git_diff` status, `git_log`
    commits); without it, error. A directory status lists every untracked file under it; the
    whole-tree status follows git's untracked mode, which shows a wholly untracked directory once.
15. **ADR-0018's `max_tokens` convention and ADR-0020's `total_is_partial` become one vocabulary:**
    `next_cursor` alone means more; a cut at `limit` is paging; a cut to the token budget sets
    `budgeted`; a cut the caller did not ask for sets `truncated` and `truncated_reason`, with
    `omitted` where known; flags are absent when false (the `memory_search` listing is the exception
    named under Consequences). Reasons: `scan_cap`, `byte_budget`,
    `imports`, `size_cap`, `depth`, `rerank_top_k`, `filter_depth`, `path_cap`, `hunk_cap`,
    `byte_cap`, `window`, `shallow_clone`, `live_walk`. Distinct concepts keep their names:
    `skipped_files`, `limit_clamped`, `body_truncated`, `too_large`, `index_status.partial`.
16. **ADR-0018's paging convention adds:** a cursor is bound to its tool and every argument but
    `limit`, `max_tokens` and `cursor`. A foreign cursor, or one that outlived its index generation,
    HEAD or history source, is an error, never an empty page. `limit: 0` is refused.
17. **ADR-0022 Contract 5's empty page extends to `git_log`:** a path or symbol history inspects one
    window of commits per page and may return none with a `next_cursor` (`window`). A name no
    inspected commit matches is listed in `unmatched` and is an error on the last page, unless the
    clone is shallow. A path no commit reachable from HEAD touches and that is not on disk is an
    error, a directory only on the last page, again unless the clone is shallow. A query before the
    history index is ready searches the newest 10,000 commits.
18. **ADR-0018's "a `notice` reports ... a degraded lane" becomes:** `notice` reports index state
    only (`warming_up`, `building_index`, `rescanning`, `index_rebuilding`, `empty_index`,
    `projections_capped`); a degraded lane is in `lanes_unavailable`; git tools have no `notice`.
19. **ADR-0018's "failures name the fix" covers stale input:** a `code_grep` or `find_files` filter
    that selects no file is an error unless a retry notice says files may be missing. A file edited
    since the scan is served as it is and marked `stale`; where the index cannot answer for it, or
    it is gone, the error names `index_rescan`. No tool loads a file past `scan.max_file_bytes`
    whole, and `code_grep` follows no link out of the repository unless `scan.follow_symlinks` is
    set.

### Stores

20. **ADR-0025 (a), "a newer version on disk is never wiped", extends to collection:** an older
    binary's GC never marks from, evicts or heals a newer view, and leaves newer blob namespaces and
    newer-view workspaces out of its budget and sweep. This and rule 21 bind binaries built from
    this rule on (0.34); 0.33 and earlier have no such GC and know no newer view or namespace.
21. **ADR-0025 (a) on blob paths, and the cost in ADR-0025 that "garbage collection must know which
    schema versions are still live", become:** blobs live in `blobs/v<schema>/`; a file map records
    its `blob_layout`, and one of an earlier layout is rebuilt whatever the minor. Older namespaces
    and pre-namespace blobs are reaped past the 6-hour blob grace, by age alone; newer ones are
    never touched. Reason: blobs are derived, and an install still in use re-extracts what it lost.
22. **ADR-0025 (b), "the older binary refuses to open the store at all", and ADR-0024's consequence
    that it serves nothing, become:** facing a newer notes store it serves the code index and
    refuses memory operations only (a newer code index is still refused), because the notes are
    shared by every worktree of a group. A writer open that would wipe a view still holding legacy
    memory rows is refused. Only a writer open stamps the version.
23. **ADR-0025 (c) and its vectors row become:** memory-note vectors live per group and embedding
    preset in `repos/<group>/memory.lance/<model>-<dim>-<hash>/`; rows hold no text. Each search
    embeds at most 256 missing notes or 3 seconds' worth, and the next continues. A save that wrote
    no row rewrites the group's stamp (`memory-rows-owed`), which every table reads; one that wrote
    its row in its own preset's table rewrites the stamp (`memory-rows-owed.<table>`) of every other
    table directory present at that moment, a preset no longer in use included. A search reads both
    before it trusts its table to be level. Stamps are renamed into place whole and never collected;
    a save that dies between its row and the stamps leaves other presets to find the note when
    their level lapses, within an hour.
24. **ADR-0025 (d), "a pending set recorded in the file map", becomes:** the set is the sidecar
    `pending-vectors.msgpack`, synced before the file map that claims its files, so settling a path
    costs one small write. A view records its `vector-store-id`; a lost or replaced store, or an
    unclean shutdown, re-owes every embedded path from blobs. `index_rescan {}` is an incremental
    scan of the whole workspace and forces no verification; a pending `verify-blobs` still makes
    it full.
25. **ADR-0025 (d) extends to blobs,** written without fsync: a leftover scan breadcrumb marks its
    workspace `verify-blobs`, and a `<data home>/hosting/<pid>` marker (boot id, process start) of a
    daemon that died with the machine (in an earlier boot, or gone where the boot cannot be told)
    marks every workspace, since it does not say which it served; one that died alone marks none.
    A worktree seeded from a workspace with either is marked too. A marked workspace's next scan
    is full and validates each file-map and document blob it would otherwise trust for existing.
26. **ADR-0025's store opens add:** an open without the workspace lock writes nothing; a workspace
    directory is renamed into `cache/.reaped/` under its lock before it is deleted. A vector
    store's open holds its `.open.lock` while it wipes and creates; one that waits over 30 s
    fails, and the scan pass or, for a minute, the tool calls after it take the store as
    unavailable instead of waiting again, those already queued behind the failed open included.
    Hybrid `code_search` then names the vector lane in `lanes_unavailable`; `docs_search`,
    `memory_search` by `query` and `code_search` with `mode: "semantic"` refuse the call as an
    `isError` result naming the store unavailable, its cause and the retry, with what the caller
    can use meanwhile. An open that cannot create the lock file goes on unserialised.

## Consequences

Open, as built:

- CLI `scan` does not forward (ADR-0024 rule 5): with another process holding the writer lock it
  stops with a notice naming it, which ADR-0024's consequences said it must not.
- Rule 7: the lock is not re-checked before SIGKILL, and without start times a process given a dead
  daemon's pid is not ruled out. A reused pid in a stale record of any generation counts toward
  the ceiling while it runs: this generation's records are pruned only when their pid is dead,
  and another generation's never by this binary.
- `agents` (8,000 bytes) stands in place of ADR-0022's three agent tools until comms v2 lands.
- The `memory_search` listing sets `truncated` with no `truncated_reason`, outside rule 15. A
  `code_refs` definition answered by the daemon is not one snapshot with the session's store.
- Only a daemon writes a hosting marker (rule 25): a scan outside it that finished before a power
  loss leaves no evidence, and its blobs are trusted for existing.
- Blob verification skips code-chunk sidecars and `rref` blobs. Owed vector rows settle only while
  a scan runs; vector stores of unused presets are never collected.

Costs: a launcher keeps each version 14 days past its last use; `comms stop` can kill a daemon a
takeover would leave alone; an older install sharing the cache re-extracts blobs older than six
hours, where ADR-0025's costs had two installed minors each keep a full set.

## Alternatives considered

- **Edit the records in place** — rejected: accepted records are immutable.
- **Supersede ADR-0024 and ADR-0025** — rejected: their decisions stand; these rules refine them.
- **One amending record per series** — rejected: the series meet in the relay, the comms protocol
  number and the memory store, and would state those rules several times.
- **Revert the code to the older text** — rejected per rule: each records a reason the older text
  did not weigh.
