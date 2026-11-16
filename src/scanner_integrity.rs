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

/// Run before a full scan touches the index: forget a full-disk latch from an earlier pass (the
/// space check below decides whether it is cured), refuse a poisoned handle outright, empty an index
/// that is behind its file map so the pass rebuilds it, and refuse a volume without room for the
/// index.
pub(crate) fn preflight(store: &mut Store) -> Result<(), ScanError> {
    crate::scan_evidence::settle_dead_hosts();
    store.retry_vector_store();
    absorb_owed_rows(store);
    if let Some(db) = store.index_db.as_ref() {
        db.clear_recoverable_fault();
        if let Some(reason) = db.fatal_fault() {
            return Err(ScanError::IndexFault(reason));
        }
    }
    store.open_index_owed_to_scan()?;
    store.rebuild_index_if_behind()?;
    ensure_space(store)
}

/// For a store opened without the workspace lock and now scanned in place: take in the owed set
/// as it is on disk, which another process may have added to since this store loaded it. A store
/// that holds the lock is the only writer of the set and has nothing to take in.
pub(crate) fn absorb_owed_rows(store: &mut Store) {
    if !store.holds_writer_lock() {
        let on_disk = crate::store_pending::PendingVectors::load(&store.view_dir);
        store.pending_vectors.absorb(on_disk);
    }
}

/// Whether an incremental scan must hand over to a full one: only a full pass can rebuild an index
/// that is behind its file map (see [`preflight`]).
pub(crate) fn needs_full_rebuild(store: &Store) -> Result<bool, ScanError> {
    Ok(store.index_is_behind_file_map()? || store.index.rebuild_pending || store.index_rebuild_needed)
}

/// [`purge_file`] for every path of `rels`, carrying on past a failure: one path whose rows could
/// not be removed must not keep the others, or the file map, from being written. Returns the paths
/// purged and the first failure; the caller flushes the file map, which still lists the failed
/// paths, and then returns that failure.
pub(crate) fn purge_files<'a>(store: &mut Store, rels: &'a [String]) -> (Vec<&'a String>, Option<ScanError>) {
    let mut purged = Vec::with_capacity(rels.len());
    let mut failure = None;
    for rel in rels {
        match purge_file(store, rel) {
            Ok(()) => purged.push(rel),
            Err(error) => {
                tracing::warn!(%rel, %error, "could not purge a removed file; it stays in the file map");
                failure.get_or_insert(error);
            }
        }
    }
    (purged, failure)
}

/// Drop a file that is gone: its index rows first, then its file-map entry. A failed commit is
/// returned with the entry still in place: a file dropped from the map over rows that stayed would
/// never be revisited, because no scan looks at a path the map does not list.
fn purge_file(store: &mut Store, rel: &str) -> Result<(), ScanError> {
    if let Some(idx) = store.index_db.as_ref() {
        let path = crate::path::RelPath::from(rel);
        let mut writer = idx.writer();
        let staged = writer
            .remove_file(&path)
            .and_then(|()| writer.remove_resolved_file(&path));
        #[cfg(feature = "code-search")]
        let staged = staged.and_then(|()| writer.remove_bm25_file(&path));
        staged
            .and_then(|()| writer.commit())
            .map_err(crate::store::StoreError::from)?;
    }
    store.remove(rel);
    Ok(())
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
/// complete only when `resolved` says every commit landed and its rows are synced. A pass that
/// panicked or lost a non-fatal commit leaves the marker unset and the scan degraded rather than
/// failed.
pub(crate) fn finish_resolve(store: &Store, resolved: bool) -> Result<(), ScanError> {
    let files = store.index.files.len();
    abort_on_fault(store, files, files)?;
    if resolved && sync_index_tail(store) {
        store.write_status_sidecar_with(&ScanHealth::complete());
    }
    Ok(())
}

/// Persist the file map (`index.msgpack`) — the durability barrier that must run BEFORE the optional
/// post-extraction lanes.
///
/// Blobs and the Fjall index are committed per file, but the file map is a single msgpack rewrite at
/// the end; [`Store::flush`] fsyncs the index journal before that rewrite, once per scan. When it was
/// written *after* the optional lanes, any lane that panicked (the
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
pub(crate) fn flush_code_map(store: &mut Store, health: &ScanHealth) -> Result<(), ScanError> {
    store.flush()?;
    // Refresh the cheap statusline sidecar the moment the code map is durable, so a shell
    // statusline sees fresh counts without ever opening the Fjall index. Best-effort by design.
    store.write_status_sidecar_with(health);
    Ok(())
}

/// The doc-removal lane, then the flush it owes for the `store.index` entries it dropped (see
/// [`flush_code_map`]). Nothing runs when no document was removed.
#[cfg(feature = "documents")]
pub(crate) fn remove_docs_and_flush(
    store: &mut Store,
    config: &crate::config::Config,
    filters: &crate::scanner_filter::Filters,
    scope: &str,
    removed: &[String],
) -> Result<(), ScanError> {
    if removed.is_empty() {
        return Ok(());
    }
    crate::scanner_lanes::run_optional_lane(crate::scanner_lanes::LANE_DOC_REMOVALS, || {
        crate::scanner_flush::flush_doc_removals_if_any(store, config, filters, scope, removed);
    });
    store.flush()?;
    Ok(())
}

/// Make the index rows a scan wrote after [`flush_code_map`] durable (the resolve pass, BM25
/// statistics): the file map already calls their files unchanged, so a row lost to a power cut would
/// never be rewritten. One journal fsync, and none when nothing was written since the last one.
/// Returns whether the rows are durable. A failure is not fatal: the code map is durable, and a
/// poisoned handle is latched for the next scan's preflight to refuse.
pub(crate) fn sync_index_tail(store: &Store) -> bool {
    let Some(db) = store.index_db.as_ref() else {
        return true;
    };
    db.sync_tail()
        .inspect_err(|error| tracing::warn!(%error, "could not fsync the index after the resolve pass"))
        .is_ok()
}

/// The last index step of a scan: [`sync_index_tail`], then release the sealed index journals now
/// that every row is durable. A failed reclaim only logs: the rows are already durable in the
/// journal. Runs for full and incremental scans alike, and costs nothing unless fjall holds a sealed
/// journal (see [`IndexDb::reclaim_journal_if_grown`]).
pub(crate) fn finish_index_writes(store: &Store) {
    sync_index_tail(store);
    if let Some(db) = store.index_db.as_ref() {
        db.reclaim_journal_if_grown();
    }
}

/// Drop the grep bloom rows of every content no indexed file references any more. Rows are shared by
/// content, so a removed or edited file leaves its old row behind until this full-pass sweep (or, for
/// a watcher batch, [`drop_unheld_grep_bloom_rows`]).
pub(crate) fn sweep_grep_bloom_rows(store: &Store) {
    let Some(idx) = store.index_db.as_ref() else {
        return;
    };
    let live: ahash::AHashSet<&str> = store
        .index
        .files
        .values()
        .map(|entry| entry.hash_hex.as_str())
        .collect();
    if let Err(error) = idx.sweep_grep_bloom(&live) {
        tracing::warn!(%error, "grep bloom sweep failed; unreferenced rows stay until the next full scan");
    }
}

/// Drop the grep bloom rows of `hashes` that no indexed file holds any more. `hashes` are the
/// contents the pass's changed or removed paths held before it ran, so the cost follows the batch,
/// not the index; a hash another path still holds (or one the pass just wrote for a new path) stays.
/// Runs after the pass has applied its entries and committed its rows, so it sees the final manifest.
pub(crate) fn drop_unheld_grep_bloom_rows(store: &Store, hashes: &[String]) {
    let Some(idx) = store.index_db.as_ref() else {
        return;
    };
    let unheld: ahash::AHashSet<&str> = hashes
        .iter()
        .map(String::as_str)
        .filter(|hash| !store.hash_is_held(hash))
        .collect();
    if unheld.is_empty() {
        return;
    }
    if let Err(error) = idx.remove_grep_bloom_rows(unheld) {
        tracing::warn!(%error, "grep bloom row removal failed; unreferenced rows stay until the next full scan");
    }
}

#[cfg(test)]
#[path = "scanner_integrity_tests.rs"]
mod tests;
