# ADR-0015: Bound document extraction with a thread cap, tombstones and a phase budget

- **Status:** Proposed
- **Date:** 2026-10-11
- **Deciders:** Na'aman Hirschfeld
- **Related:** ADR-0008, issue #72

## Context

`extract_doc` runs xberg on its own thread under a wall-clock watchdog (`[documents]
extraction_timeout_secs`). The work is CPU-bound inside a synchronous call, so a timed-out thread cannot be
cancelled: it is abandoned and keeps a core busy until it finishes. Before this change three things
compounded: nothing bounded how many abandoned threads piled up, a timed-out document left no record so
every scan paid the full timeout (and stranded another thread) again, and the 600 s default meant several
documents timing out together stalled a scan for hours.

## Decision

- **Cap abandoned threads.** `doc_guard::AbandonedThreads` counts abandoned threads that have not finished.
  `[documents] max_abandoned_extractions` (default 4) caps them: at the cap `run_with_deadline` spawns
  nothing and the document is deferred (`FileStatus::DocDeferred`, `docs_deferred`), to be picked up by a
  later scan. The live count is logged on every timeout and reported as `abandoned_extractions`. The cap is
  soft: extractions already in flight may still time out past it.
- **Tombstone timed-out documents.** `Index::doc_tombstones` records the content hash and
  `config::rules::doc_guard_digest` (per-file timeout, chunk cutovers, `max_pages`, the extraction digest)
  for a document that timed out. While both match, the file is skipped (`docs_tombstoned`). Editing the file
  or changing a limit retries it. The field is `#[serde(default)]`, so old indexes read unchanged; the full
  scan prunes tombstones of deleted files and a successful extraction removes its own.
- **Budget the phase.** `[documents] phase_budget_secs` (default 1800, never below the per-file timeout)
  starts at the first document that needs a real extraction; once spent, later documents are deferred, not
  failed. The default per-file timeout drops from 600 s to 120 s.

## Consequences

Stuck extractions can no longer accumulate without bound, and an unchanged pathological file costs one
timeout per set of limits, not one per scan. Deferred documents keep their previous index entry until a scan
reaches them. A document that legitimately needs more than 120 s (a large scanned PDF) now times out until
the user raises `extraction_timeout_secs`; the tombstone makes that a one-time cost rather than a recurring
one. The phase budget can overshoot by up to one per-file timeout, because running documents are not
interrupted.

## Alternatives considered

- **Run extraction in a killable subprocess.** Evaluated, rejected for now. Extraction depends on
  in-process state (the shared Tokio runtime, the ONNX embedder and xberg's loaded models), so a child must
  reload models per document or be a long-lived worker pool with its own protocol. The extracted document,
  chunks and embedding vectors would cross a pipe as a serialised blob. The memory-footprint gate cannot see
  child processes. It needs a self-exec path, kill and reap handling, and a Windows variant. That is far from
  small; the cap bounds the damage at a fraction of the cost. Worth revisiting if xberg offers cooperative
  cancellation (which would make the thread cap unnecessary anyway).
- **Key tombstones by content hash alone.** Would also catch a renamed or copied file, but needs reference
  counting to prune; path-keyed entries are pruned with their file, and the hash inside still makes an edit
  retry.
- **Clamp the per-file timeout to the remaining phase budget.** Would turn a healthy slow document into a
  false timeout (and tombstone) just because the budget ran low; deferring is the honest outcome.
