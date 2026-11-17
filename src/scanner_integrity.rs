//! What a scan must establish before it may call the index complete.
//!
//! Three failure modes used to leave a status that read complete over a damaged index: the disk
//! filling mid-pass (issue #79), a poisoned index handle failing every later commit (#79), and a
//! cancel that flushed a partial file map (#84). The checks live here, outside `scanner.rs`, so the
//! scan body stays a readable sequence of phases and the module stays under the line cap.

use crate::disk_space;
use crate::index::IndexDb;
use crate::scanner::ScanError;
use crate::store::{ScanHealth, Store, StoreError};

/// Run before a full scan touches the index: take the writer's place ([`claim_writer`]), forget a
/// full-disk latch from an earlier pass (the space check below decides whether it is cured), refuse
/// a poisoned handle outright, empty an index that is behind its file map so the pass rebuilds it,
/// refuse a volume without room for the index, and open the pass's rows ([`open_full_pass`]).
pub(crate) fn preflight(store: &mut Store) -> Result<UnlistedRows, ScanError> {
    crate::scan_evidence::settle_dead_hosts();
    store.retry_vector_store();
    claim_writer(store)?;
    if let Some(db) = store.index_db.as_ref() {
        db.clear_recoverable_fault();
        if let Some(reason) = db.fatal_fault() {
            return Err(ScanError::IndexFault(reason));
        }
    }
    store.rebuild_index_if_behind()?;
    ensure_space(store)?;
    open_full_pass(store)
}

/// Whether an earlier pass may have left rows of paths no file map lists, for a full pass to remove
/// once its drive has listed every path it saw ([`UnlistedRows::purge`]).
#[must_use = "a full pass must purge what an interrupted pass left"]
pub(crate) struct UnlistedRows(bool);

impl UnlistedRows {
    /// Remove, after the drive and the stale purge, the rows of every path the file map does not
    /// list: the drive put every path it saw in the map, so what is left is what no pass will look
    /// at again.
    pub(crate) fn purge(self, store: &Store) -> Result<(), ScanError> {
        match (self.0, store.index_db.as_ref()) {
            (true, Some(db)) => purge_unlisted_rows(store, db),
            _ => Ok(()),
        }
    }
}

/// Run before an incremental pass commits its first index row: remove the rows an earlier pass may
/// have left of paths no file map lists, then record that a pass is open
/// ([`crate::index::IndexDb::open_rows_pass`]). A pass killed between committing rows and writing
/// the file map that lists them leaves rows that no scan ever revisits, since none looks at a path
/// the map does not list; served by references, symbols and search for files that may no longer
/// exist. Only the flush that ends a pass clears the record ([`flush_code_map`] with `ends_pass`,
/// [`Store::flush_ending_rows_pass`]); a seal that wrote no rows leaves it.
pub(crate) fn begin_rows(store: &mut Store) -> Result<(), ScanError> {
    let Some(db) = store.index_db.clone() else {
        return Ok(());
    };
    if db.rows_may_be_unlisted().map_err(StoreError::from)? {
        purge_unlisted_rows(store, &db)?;
    }
    db.open_rows_pass().map_err(StoreError::from)?;
    Ok(())
}

/// [`begin_rows`] for a full pass, whose purge waits for its drive: before it, the file map of a
/// pending rebuild lists nothing, and every path would be purged only to be indexed again.
fn open_full_pass(store: &mut Store) -> Result<UnlistedRows, ScanError> {
    let Some(db) = store.index_db.as_ref() else {
        return Ok(UnlistedRows(false));
    };
    let owed = db.rows_may_be_unlisted().map_err(StoreError::from)?;
    db.open_rows_pass().map_err(StoreError::from)?;
    Ok(UnlistedRows(owed))
}

/// Remove the index rows of every path the file map does not list.
fn purge_unlisted_rows(store: &Store, db: &IndexDb) -> Result<(), ScanError> {
    let mut unlisted: Vec<_> = db.indexed_paths().map_err(StoreError::from)?.into_iter().collect();
    unlisted.retain(|rel| store.lookup(rel).is_none());
    if !unlisted.is_empty() {
        tracing::info!(
            paths = unlisted.len(),
            "removing index rows an interrupted scan left of files no file map lists"
        );
    }
    db.purge_paths(&unlisted).map_err(StoreError::from)?;
    Ok(())
}

/// Make this store the workspace's writer before a scan writes into it: a store opened without the
/// workspace lock takes it, or the scan is refused while another process holds it (see
/// [`Store::take_writer_lock`]), and the store gets the index handle its open did not take. Having
/// just taken the lock, it takes in the owed set as it is on disk, which another process may have
/// added to since this store loaded it. A store that held the lock from its open is the only
/// writer of the set and has nothing to take in.
///
/// The index handle a read-only open took stamps, migrates and sweeps nothing, so a store that just
/// took the lock drops it and opens the index as a writer does. While a reader still holds a clone
/// of that handle fjall's lock cannot be taken again, so the scan writes through the handle it has.
pub(crate) fn claim_writer(store: &mut Store) -> Result<(), ScanError> {
    if store.take_writer_lock()? {
        let on_disk = crate::store_pending::PendingVectors::load(&store.view_dir);
        store.pending_vectors.absorb(on_disk);
        if store.index_db.as_ref().is_some_and(IndexDb::is_sole_handle) {
            store.index_db = None;
        }
    }
    store.open_index_owed_to_scan()?;
    Ok(())
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
        purge_rows(idx, &crate::path::RelPath::from(rel))?;
    }
    store.remove(rel);
    Ok(())
}

/// Remove every index row of `path`, in one commit.
fn purge_rows(db: &IndexDb, path: &crate::path::RelPath) -> Result<(), StoreError> {
    db.purge_paths(std::slice::from_ref(path)).map_err(StoreError::from)
}

/// The free-space check alone, for the resolve pass: the file map is already durable by then, so a
/// refusal costs only the resolve tier.
pub(crate) fn ensure_space(store: &Store) -> Result<(), ScanError> {
    disk_space::ensure_free_space(&crate::store_layout::group_index_dir(&store.root))?;
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
/// `ends_pass` when the map now lists every row the pass wrote (see [`begin_rows`]): never for a
/// cancelled pass or one whose purge failed, which the next pass then checks.
///
/// A lane that mutates `store.index` must flush again after itself (only the doc-removal lane does).
pub(crate) fn flush_code_map(store: &mut Store, health: &ScanHealth, ends_pass: bool) -> Result<(), ScanError> {
    if ends_pass {
        store.flush_ending_rows_pass()?;
    } else {
        store.flush()?;
    }
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

/// Drop the grep bloom rows of every content no indexed file of the group references any more (see
/// [`crate::store_group_live`]). Rows are shared by content, so a removed or edited file leaves its old row behind until this full-pass sweep (or, for
/// a watcher batch, [`drop_unheld_grep_bloom_rows`]).
pub(crate) fn sweep_grep_bloom_rows(store: &Store) {
    let Some(idx) = store.index_db.as_ref() else {
        return;
    };
    let liveness = match store.content_liveness() {
        Ok(liveness) => liveness,
        Err(error) => {
            tracing::warn!(%error, "grep bloom sweep skipped; unreferenced rows stay until the next full scan");
            return;
        }
    };
    if let Err(error) = idx.sweep_grep_bloom(|hash| liveness.holds(hash)) {
        tracing::warn!(%error, "grep bloom sweep failed; unreferenced rows stay until the next full scan");
    }
}

/// Drop the grep bloom rows of `hashes` that no indexed file of the group holds any more. `hashes`
/// are the contents the pass's changed or removed paths held before it ran, so the cost follows the
/// batch, not the index; a hash another path still holds (or one the pass just wrote for a new path)
/// stays. Runs after the pass has applied its entries and committed its rows, so it sees the final
/// manifest.
pub(crate) fn drop_unheld_grep_bloom_rows(store: &Store, hashes: &[String]) {
    let Some(idx) = store.index_db.as_ref() else {
        return;
    };
    if hashes.iter().all(|hash| store.hash_is_held(hash)) {
        return;
    }
    let liveness = match store.content_liveness() {
        Ok(liveness) => liveness,
        Err(error) => {
            tracing::warn!(%error, "grep bloom row removal skipped; unreferenced rows stay until the next full scan");
            return;
        }
    };
    let unheld: ahash::AHashSet<&str> = hashes
        .iter()
        .map(String::as_str)
        .filter(|hash| !liveness.holds(hash))
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
