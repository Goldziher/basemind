//! What a scan must establish before it may call the index complete.
//!
//! Three failure modes used to leave a status that read complete over a damaged index: the disk
//! filling mid-pass (issue #79), a poisoned index handle failing every later commit (#79), and a
//! cancel that flushed a partial file map (#84). The checks live here, outside `scanner.rs`, so the
//! scan body stays a readable sequence of phases and the module stays under the line cap.

use crate::disk_space;
use crate::index::IndexDb;
use crate::scanner::ScanError;
use crate::store::{ScanHealth, Store};

/// Run before a scan touches the index: forget a full-disk latch from an earlier pass (the space
/// check below decides whether it is cured), refuse a poisoned handle outright, and refuse a volume
/// without room for the index.
pub(crate) fn preflight(store: &Store) -> Result<(), ScanError> {
    if let Some(db) = store.index_db.as_ref() {
        db.clear_recoverable_fault();
        if let Some(reason) = db.fatal_fault() {
            return Err(ScanError::IndexFault(reason));
        }
    }
    ensure_space(store)
}

/// The free-space check alone, for the resolve pass: the file map is already durable by then, so a
/// refusal costs only the resolve tier.
pub(crate) fn ensure_space(store: &Store) -> Result<(), ScanError> {
    disk_space::ensure_free_space(&store.view_dir)?;
    Ok(())
}

/// Abort when an index commit failed fatally. Records the scan as partial in the status sidecar
/// (counts untouched: the file map in memory is ahead of the unflushed `index.msgpack`) so nothing
/// reads the half-written index as complete.
pub(crate) fn abort_on_fault(store: &Store, done: usize, total: usize) -> Result<(), ScanError> {
    let Some(reason) = store.index_db.as_ref().and_then(IndexDb::fatal_fault) else {
        return Ok(());
    };
    store.mark_status_health(&ScanHealth::partial(
        format!("index commit failed: {reason}"),
        done,
        total,
    ));
    Err(ScanError::IndexFault(reason))
}

/// Close out the resolve pass: abort on a fatal commit failure, otherwise record the resolve tier
/// complete only when `resolved` says every commit landed. A pass that panicked or lost a
/// non-fatal commit leaves the marker unset and the scan degraded rather than failed.
pub(crate) fn finish_resolve(store: &Store, resolved: bool) -> Result<(), ScanError> {
    let files = store.index.files.len();
    abort_on_fault(store, files, files)?;
    if resolved {
        store.write_status_sidecar_with(&ScanHealth::complete());
    }
    Ok(())
}

/// Persist the file map (`index.msgpack`) — the durability barrier that must run BEFORE the optional
/// post-extraction lanes.
///
/// Blobs and the Fjall index are committed per file, but the file map is a single msgpack rewrite at
/// the end. When it was written *after* the optional lanes, any lane that panicked (the
/// `stack-graphs` stitcher) or hung until the operator killed the process (the embedding-model
/// download on a blackholed IPv6 route) left the workspace with gigabytes of committed blobs beside
/// an `index.msgpack` reporting `file_count: 0` — a silently empty code map that forced a full
/// re-scan on every launch. Flushing here makes the code map durable the moment it is complete;
/// every lane after this point is enrichment, and a lane that dies costs only its own tier.
///
/// `health` is what the sidecar says about the scan at this barrier: [`ScanHealth::resolving`] for a
/// finished walk whose resolve pass is still to come, [`ScanHealth::partial`] for a cancelled one.
///
/// A lane that mutates `store.index` must flush again after itself (only the doc-removal lane does).
pub(crate) fn flush_code_map(store: &Store, health: &ScanHealth) -> Result<(), ScanError> {
    store.flush()?;
    // Refresh the cheap statusline sidecar the moment the code map is durable, so a shell
    // statusline sees fresh counts without ever opening the Fjall index. Best-effort by design.
    store.write_status_sidecar_with(health);
    Ok(())
}
