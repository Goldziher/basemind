---
status: accepted
date: 2026-11-15
---

# ADR-0025: Store versioning and durability policy

- **Deciders:** basemind maintainers
- **Related:** ADR-0016, ADR-0021, ADR-0024, ADR-0026

## Context

basemind keeps a dozen kinds of persistent state. Each grew its own version constant, its own rule for
a version it does not recognise, and its own durability, and no record says which is right. Two
facts make the differences matter: several basemind versions run against one cache at once
(ADR-0024), and two of the stores hold data a user or agent wrote that nothing can regenerate.

The table is the state of `main`. Paths are relative to the cache root
(`<data dir>/cache/`, `src/store_layout.rs`) unless they start with `comms/` or `<data dir>/`.
"Minor" is `RELEASE_MINOR` in `src/version.rs`, 33 today. "Older" and "newer" compare the version on
disk with the binary's. "Not verified" marks a cell the audit did not check in the code.

| Store | Path, engine | Content | Version | Older / newer on disk | Durability of a write |
|---|---|---|---|---|---|
| File map | `workspaces/<ws>/views/<view>/index.msgpack`, msgpack | derived | `extract::SCHEMA_VER` = minor | rebuild / refused | temp file, fsync, rename, after the index seal |
| Code index | `…/views/<view>/index.fjall`, Fjall | derived | `INDEX_SCHEMA_VER` = minor + 6 | wiped on open / refused | buffered; one synced commit per scan seal and one sync at scan end |
| Blobs | `blobs/<content hash>.<kind>.msgpack`, kinds `fm`, `chunk`, `rref`, `doc`; compressed msgpack | derived | `extract::SCHEMA_VER`, in each `fm` blob; other kinds not verified | rebuild / refused | temp file, rename, no fsync |
| Base store (ADR-0016) | `repos/<group>/base/gen-<n>/`, sorted runs with a `base.json` manifest | derived | index and blob schema versions, in the manifest | a generation of any other version is not used | files and manifest synced |
| Memory notes | `repos/<group>/memory.fjall`, Fjall | user-authored | none | no check | buffered, no sync |
| Vectors | `workspaces/<ws>/lance/`, LanceDB | derived | `MEMORY_SCHEMA_VER` = minor, with dimension and model, in `meta.json` | directory wiped / refused | LanceDB's commit; not synced by basemind |
| Git history | `workspaces/<ws>/git-history.fjall`, Fjall | derived | `GIT_HISTORY_SCHEMA` = minor + 6 | wiped on open / refused | buffered, no sync |
| Git cache | `workspaces/<ws>/git-cache/`, msgpack files | derived | `GIT_CACHE_SCHEMA` = minor + 1 | recomputed and overwritten, in both directions | temp file, rename, no fsync |
| Agent store | `comms/store.fjall`, Fjall | user-authored | `COMMS_SCHEMA_VER` = minor | **wiped on open** / refused | buffered, no sync |
| Status sidecar | `workspaces/<ws>/status.json`, JSON | derived | `STATUS_SIDECAR_SCHEMA_VER` = 1 | ignored, in both directions | temp file, rename, no fsync |
| Registry snapshot | `<data dir>/registry/registry.msgpack`, msgpack | derived | `REGISTRY_SCHEMA_VER` = 1 | read as empty, then overwritten, in both directions | temp file, rename, no fsync |
| Telemetry | `workspaces/<ws>/telemetry.jsonl`, JSON lines | log | none | no check | append, no fsync |

Blob files with the pre-0.9 suffixes `l1` and `l2` are dead format, deleted on sight by the blob
sweep (`src/store_gc.rs`).

Three things in the table are defects, not variety. The agent store is user-authored and is erased
by every minor release. Memory notes have no version, so no later binary can tell what layout it is
reading. And blobs are addressed by content hash alone, so binaries of two minors read and write the
same file: the newer treats a blob the older wrote as stale, and the older refuses one the newer
wrote (`check_schema` in `src/store.rs`).

One crash-consistency mechanism exists and works: `Store::flush` (`src/store.rs`) seals a scan
generation in the code index with a synced commit before it writes the file map carrying the same
generation, and a writer that finds the file map ahead of the index discards the index and empties
the file map (`src/store_durability.rs`).

## Decision

Every store is classified as derived or user-authored, and the class decides its rules.

**(a) Derived stores** — file map, code index, blobs, base store, vectors, git history, git cache,
status sidecar, registry snapshot:

- The version is tied to the release minor, with a fixed offset where a store needs one. **Every
  minor release therefore discards these stores and rescans**, automatically, whether or not a
  format changed. The status sidecar and the registry snapshot keep their own constant, because a
  mismatch costs nothing: they are rewritten by the next scan or registration.
- An **older** version on disk is discarded and rebuilt. It is never served.
- A **newer** version on disk is never wiped and never overwritten by an older binary. The older
  binary refuses the store with an error that says it was written by a newer basemind.
- **Blobs only: the path includes the schema version**, for every kind (`fm`, `chunk`, `rref`,
  `doc`). Blobs are global: one directory shared by every workspace and every installed version,
  written without a lock, so two versions meet in it constantly. The other derived stores are per
  workspace or repository group and are opened under a lock; there the refusal above is enough.
- No fsync is required beyond what clause (d) needs: a lost derived record is rebuilt.

**(b) User-authored stores** — memory notes and agent messages:

- Each has its own version constant, independent of the release.
- Layout changes are forward-only migrations run at open. The store is never wiped by a version
  difference, older or newer. A protocol generation change (ADR-0024) is not a version difference:
  the agent store of a new generation starts empty. Memory notes are not generation-scoped.
- A newer store is not written by an older binary. For memory notes the older binary refuses to
  open the store at all, the simplest rule; the agent store opens for reading (ADR-0021).
- The memory-notes store has no version today. It gains one the first time a binary that has the
  constant opens it; binaries older than that cannot see the version and are not held by this rule.
- A write is synced to disk before the call returns where its loss cannot be recovered. That is
  the case for memory notes. Agent messages are the stated exception: ADR-0021 commits a post with
  buffered persist and accepts losing the last posts on power loss.

**(c) A derived store built from a user-authored one** — the vector rows of memory notes:

- After a wipe it is rebuilt from the notes, lazily, by the first search by meaning that finds the
  vector table missing or behind the notes.
- It is never the source of a value returned to a caller. It supplies keys and ranks; the value is
  read from the user-authored record.

**(d) Crash consistency.** The scan-generation seal is the contract between the code index and the
file map: both carry the same scan generation, the index is synced first, and a file map that is
ahead of its index is discarded together with that index. Vectors are covered by a pending set
recorded in the file map: the files whose vector rows are not yet committed are written there
before they are embedded and cleared after the commit, so an interrupted scan knows what to redo.

Telemetry is a log, in neither class: unversioned, append-only, and safe to delete.

Fixes are in progress on the branch `fix/storage-audit`. This record is the policy they are held
to.

## Consequences

Code on `main` that does not meet the policy:

- **Agent store** — versioned by the release minor and wiped when older, against (b). ADR-0021
  replaces it with `agents.fjall` under its own `COMMS_STORE_VER`.
- **Memory notes** — no version constant and no migration list, against (b). One hand-written
  migration exists (`migrate_legacy` in `src/index/memory_store.rs`).
- **Memory notes** — `memory_save` inserts with Fjall's default persist and no sync
  (`write_record` in `src/mcp/memory_ops.rs`), against (b): a power loss can lose a note whose save
  was acknowledged.
- **Memory vectors** — a search by meaning returns `value` from the vector row (`run_memory_search`
  in `src/mcp/memory.rs`), against (c). No code was found that re-embeds notes after the vector
  directory is wiped, so after a minor upgrade notes exist but are not found by meaning until they
  are saved again.
- **Blobs** — one path per content hash for every schema version, against (a).
- **Git cache, status sidecar, registry snapshot** — a newer payload is overwritten by an older
  binary, against (a). The cost is recomputation while two minors alternate; no data is lost.
- **Vectors** — no pending set was found on `main`.

Costs of the policy itself:

- **A rescan at every minor release**, for every workspace, including minors that change no
  format. Decoupling each derived version from the minor would avoid it and is not decided here.
- Versioned blob paths mean two installed minors each keep a full set of blobs for the same files.
  Disk use for blobs doubles while both are in use, and garbage collection must know which schema
  versions are still live.
- **Two versions alternating on one workspace:** once the newer has scanned it, the older refuses
  that workspace until the user upgrades or removes the store. Refusing is chosen over rebuilding,
  because the newer binary would wipe the rebuilt store in turn, and the two would rebuild in a
  loop. The same holds for memory notes once a newer binary has migrated them.
- Until the first search by meaning after a wipe, memory notes are found by key and prefix only.
- Syncing every memory save adds one fsync per save.
- Reading memory values from the user-authored record adds one Fjall lookup per search hit.
- User-authored stores now need a migration for every layout change, and each migration needs a
  test against a store written by the previous layout.

## Alternatives considered

- **One global version for every store** — rejected: a change to the index key layout would then
  discard git history and vectors it did not touch, and would have to either erase user data or
  exempt it, which is two classes again.
- **Wipe an older store of any class** — today's rule for every Fjall store that has a version,
  the agent store included. Rejected: correct for derived data and a data loss for user-authored
  data, which the agent store undergoes at every minor release.
- **Let an older binary rebuild a newer derived store** — rejected: with two versions installed
  (ADR-0024) each rebuilds what the other wrote, indefinitely.
- **SQLite for user-authored state** — rejected: no SQLite crate is linked today, it would add a C
  build to every release target, and Fjall with a version constant and migrations meets the need.
- **Sync every write of every store** — rejected: derived stores are rebuilt after a loss, and it
  would add an fsync per indexed file to a scan that today syncs once per seal.
