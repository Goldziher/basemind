//! Post-drive flush helpers for [`crate::scanner`]: push buffered document and code-chunk batches
//! into LanceDB, purge removed files' rows, maintain the tables and refresh the BM25 stats. Each has
//! a no-op twin when its feature is compiled out.

use std::path::Path;

use crate::config::Config;
use crate::scanner_drive::{PendingCodeBatchOpt, PendingDocBatchOpt};
use crate::scanner_filter::Filters;
use crate::scanner_lanes::{LANE_CODE_BATCHES, LANE_DOC_BATCHES, run_optional_lane};
use crate::store::Store;

/// Compact fragments and prune old table versions of the LanceDB store once this scan's writes are
/// done (#76). No-op when the scan never opened the store or wrote nothing.
pub(crate) fn maintain_lance_if_open(store: &Store, filters: &Filters) {
    #[cfg(any(feature = "documents", feature = "code-search"))]
    filters.retrieval.vectors.maintain(store);
    #[cfg(not(any(feature = "documents", feature = "code-search")))]
    let _ = (store, filters);
}

/// How many owed paths an incremental scan re-queues. A watcher batch that embeds one file must not
/// also re-read every blob a long outage left owing; a full pass takes them all.
pub(crate) const REQUEUE_PER_INCREMENTAL: usize = 64;

/// Write the vector rows this scan owes and up to `requeue` of the ones an earlier scan left
/// owing, then rewrite the owed set beside the file map (see [`crate::store_pending`]).
///
/// The lanes are fault-contained and report nothing: what they could not write simply stays owed.
/// Only the write of the owed set can fail.
pub(crate) fn flush_vector_lanes(
    store: &mut Store,
    config: &Config,
    filters: &Filters,
    scope: &str,
    mut doc_batches: Vec<PendingDocBatchOpt>,
    mut code_batches: Vec<PendingCodeBatchOpt>,
    requeue: usize,
) -> Result<(), crate::store::StoreError> {
    owe_rows_that_may_be_lost(store, filters)?;
    if owes_beyond(store, &doc_batches, &code_batches) && vector_store_opens(store, config, filters) {
        let mut budget = requeue;
        requeue_pending_docs(store, config, filters, scope, &mut doc_batches, &mut budget);
        requeue_pending_code(store, config, filters, &mut code_batches, &mut budget);
    }
    run_optional_lane(LANE_DOC_BATCHES, || {
        flush_doc_batches_if_any(store, config, filters, doc_batches);
    });
    run_optional_lane(LANE_CODE_BATCHES, || {
        flush_code_batches_if_any(store, config, filters, scope, code_batches);
    });
    record_vector_store(store, filters);
    store.pending_vectors.persist(&store.view_dir)
}

/// Put every indexed path back in the owed set when the rows the file map counts on may not be in
/// the vector store: the store this view's rows were written to is gone or was replaced (wiped
/// for another model or schema, deleted, its `meta.json` unreadable), or this is the pass that
/// verifies blobs after the machine went down, when LanceDB commits made shortly before may not
/// have reached the disk although their debts were settled. The re-queue rebuilds the rows from
/// the cached blobs and settles every path that has none.
///
/// The set is written before the record of the lost store is dropped: between the two, a process
/// that dies leaves the loss to be found again, not a debt nobody knows of.
#[cfg(any(feature = "documents", feature = "code-search"))]
fn owe_rows_that_may_be_lost(store: &mut Store, filters: &Filters) -> Result<(), crate::store::StoreError> {
    // A view an earlier build last scanned records no store, so only the verifying pass can
    // tell that its rows were lost.
    let lance_dir = store.basemind_dir.join(crate::store::LANCE_DIR);
    let lost = crate::store_pending::recorded_store(&store.view_dir).is_some_and(|recorded| {
        filters.retrieval.vectors.store_id_at(&lance_dir).as_deref() != Some(recorded.as_str())
    });
    if !lost && !filters.vector_rows_unsure {
        return Ok(());
    }
    if lost {
        tracing::warn!(
            view = %store.view,
            "the vector store this view's rows were written to is gone; rewriting them from the cached blobs"
        );
        // A handle kept from an earlier scan is on the store that went.
        filters.retrieval.vectors.close(store);
    }
    #[cfg(feature = "documents")]
    for rel in store.index.doc_files.keys() {
        store.pending_vectors.owe_doc(&rel.to_str_lossy());
    }
    #[cfg(feature = "code-search")]
    for rel in store.index.files.keys() {
        store.pending_vectors.owe_code(&rel.to_str_lossy());
    }
    store.pending_vectors.persist(&store.view_dir)?;
    if lost {
        let _ = std::fs::remove_file(store.view_dir.join(crate::store_pending::STORE_ID_FILE));
    }
    Ok(())
}

#[cfg(not(any(feature = "documents", feature = "code-search")))]
fn owe_rows_that_may_be_lost(_store: &mut Store, _filters: &Filters) -> Result<(), crate::store::StoreError> {
    Ok(())
}

/// Record, beside the file map, which vector store this scan had open: the one its rows went to.
#[cfg(any(feature = "documents", feature = "code-search"))]
fn record_vector_store(store: &Store, filters: &Filters) {
    if let Some(id) = filters.retrieval.vectors.open_store_id(store) {
        crate::store_pending::record_store(&store.view_dir, &id);
    }
}

#[cfg(not(any(feature = "documents", feature = "code-search")))]
fn record_vector_store(_store: &Store, _filters: &Filters) {}

/// Whether an earlier scan left a path owing rows that this scan has no batch for.
fn owes_beyond(store: &Store, doc_batches: &[PendingDocBatchOpt], code_batches: &[PendingCodeBatchOpt]) -> bool {
    let pending = &store.pending_vectors;
    #[cfg(feature = "documents")]
    let docs = pending.docs().len() > doc_batches.len()
        || doc_batches
            .iter()
            .any(|batch| !pending.docs().contains(bstr::BStr::new(batch.rel_path.as_bytes())));
    #[cfg(not(feature = "documents"))]
    let docs = {
        let _ = doc_batches;
        false
    };
    #[cfg(feature = "code-search")]
    let code = pending.code().len() > code_batches.len()
        || code_batches
            .iter()
            .any(|batch| !pending.code().contains(bstr::BStr::new(batch.rel_path.as_bytes())));
    #[cfg(not(feature = "code-search"))]
    let code = {
        let _ = code_batches;
        false
    };
    !pending.is_empty() && (docs || code)
}

/// Open the vector store before any owed path is looked at: when it cannot be opened nothing can
/// be written, and re-reading a blob per owed path would be repeated by every scan until it can.
#[cfg(any(feature = "documents", feature = "code-search"))]
fn vector_store_opens(store: &mut Store, config: &Config, filters: &Filters) -> bool {
    let opened = filters.retrieval.vectors.open(store, config);
    if let Err(error) = &opened {
        tracing::warn!(%error, owed = store.pending_vectors.len(), "vector store unavailable; owed rows stay owed");
    }
    opened.is_ok()
}

#[cfg(not(any(feature = "documents", feature = "code-search")))]
fn vector_store_opens(_store: &mut Store, _config: &Config, _filters: &Filters) -> bool {
    false
}

/// Queue a batch for documents an earlier scan left owing rows and this scan did not process, at
/// most `budget` of them. See [`crate::scan_retrieval::DocumentSink::requeue`].
#[cfg(feature = "documents")]
fn requeue_pending_docs(
    store: &mut Store,
    config: &Config,
    filters: &Filters,
    scope: &str,
    batches: &mut Vec<PendingDocBatchOpt>,
    budget: &mut usize,
) {
    filters
        .retrieval
        .documents
        .requeue(store, config, filters, scope, batches, budget);
}

#[cfg(not(feature = "documents"))]
fn requeue_pending_docs(
    _store: &mut Store,
    _config: &Config,
    _filters: &Filters,
    _scope: &str,
    _batches: &mut Vec<PendingDocBatchOpt>,
    _budget: &mut usize,
) {
}

/// Queue a batch for code files an earlier scan left owing rows and this scan did not process, at
/// most `budget` of them. The rows are rebuilt from the cached chunk sidecar, so a path owes
/// nothing, and leaves the set, when no rows can come of it: it is no longer indexed, the embed
/// policy or the chunk cap now excludes it, or its sidecar is gone, does not decode or carries no
/// vectors of the current preset. Only a sidecar that could not be read from disk stays owed.
#[cfg(feature = "code-search")]
fn requeue_pending_code(
    store: &mut Store,
    config: &Config,
    filters: &Filters,
    batches: &mut Vec<PendingCodeBatchOpt>,
    budget: &mut usize,
) {
    use crate::store::StoreError;

    if store.pending_vectors.code().is_empty() {
        return;
    }
    let cfg = &config.code_search;
    let preset = &config.documents.embedding_preset;
    let queued: ahash::AHashSet<&str> = batches.iter().map(|batch| batch.rel_path.as_str()).collect();
    let mut requeued = Vec::new();
    let mut settled = Vec::new();
    for rel in store.pending_vectors.code() {
        let rel_path = rel.to_str_lossy();
        if queued.contains(rel_path.as_ref()) {
            continue;
        }
        if *budget == 0 {
            break;
        }
        let wanted = cfg.enabled && cfg.embed && filters.code_embed_allowed(&rel_path);
        let Some(entry) = store.lookup(rel).filter(|_| wanted) else {
            settled.push(rel_path.into_owned());
            continue;
        };
        match store.peek_chunk_state(&entry.hash_hex) {
            Ok(Some(peek))
                if peek.embedding_dim > 0
                    && peek.embedding_model == *preset
                    && peek.embeddings.len() == peek.chunks.len()
                    && !crate::scanner_code::over_chunk_cap(&rel_path, peek.chunks.len(), cfg) =>
            {
                *budget -= 1;
                requeued.push(crate::scanner_code::PendingCodeBatch {
                    rel_path: rel_path.into_owned(),
                    blob_hash: entry.hash_hex.clone(),
                    embedding_dim: peek.embedding_dim,
                    bm25: Vec::new(),
                });
            }
            Err(StoreError::Io { source, .. }) => {
                tracing::warn!(rel = %rel_path, error = %source, "chunk sidecar unreadable; its vector rows stay owed");
            }
            Ok(_) | Err(_) => settled.push(rel_path.into_owned()),
        }
    }
    drop(queued);
    store.pending_vectors.settle_code(&settled);
    batches.extend(requeued);
}

#[cfg(not(feature = "code-search"))]
fn requeue_pending_code(
    _store: &mut Store,
    _config: &Config,
    _filters: &Filters,
    _batches: &mut Vec<PendingCodeBatchOpt>,
    _budget: &mut usize,
) {
}

/// Push the buffered document batches into LanceDB. No-op without the `documents` feature.
#[cfg(feature = "documents")]
fn flush_doc_batches_if_any(store: &mut Store, config: &Config, filters: &Filters, batches: Vec<PendingDocBatchOpt>) {
    if batches.is_empty() {
        return;
    }
    let _ = filters
        .retrieval
        .documents
        .flush(store, batches, &config.documents.embedding_preset);
}

#[cfg(not(feature = "documents"))]
fn flush_doc_batches_if_any(
    _store: &mut Store,
    _config: &Config,
    _filters: &Filters,
    _batches: Vec<PendingDocBatchOpt>,
) {
}

/// Purge `documents` LanceDB rows + `doc_files` entries for docs removed since the last scan. Called
/// after the batch flush so it reuses an already-open LanceStore. Only referenced under `documents`.
#[cfg(feature = "documents")]
pub(crate) fn flush_doc_removals_if_any(
    store: &mut Store,
    config: &Config,
    filters: &Filters,
    scope: &str,
    stale: &[String],
) {
    filters.retrieval.documents.remove(store, config, scope, stale);
}

/// Push the buffered code-chunk batches into LanceDB. No-op without the `code-search` feature.
#[cfg(feature = "code-search")]
fn flush_code_batches_if_any(
    store: &mut Store,
    config: &Config,
    filters: &Filters,
    scope: &str,
    batches: Vec<PendingCodeBatchOpt>,
) {
    if batches.is_empty() {
        return;
    }
    let _ = filters
        .retrieval
        .vectors
        .write_code_rows(store, scope, batches, &config.documents.embedding_preset);
}

#[cfg(not(feature = "code-search"))]
fn flush_code_batches_if_any(
    _store: &mut Store,
    _config: &Config,
    _filters: &Filters,
    _scope: &str,
    _batches: Vec<PendingCodeBatchOpt>,
) {
}

/// Purge `code_chunks` rows for files removed since the last scan. No-op without `code-search`.
/// Called after the batch flush so it reuses an already-open LanceStore.
#[cfg(feature = "code-search")]
pub(crate) fn flush_code_removals_if_any(
    store: &mut Store,
    config: &Config,
    filters: &Filters,
    scope: &str,
    stale: &[String],
) {
    crate::scanner_code::delete_stale_code_chunks(store, config, filters.retrieval.vectors, scope, stale);
}

#[cfg(not(feature = "code-search"))]
pub(crate) fn flush_code_removals_if_any(
    _store: &mut Store,
    _config: &Config,
    _filters: &Filters,
    _scope: &str,
    _stale: &[String],
) {
}

/// Retire unreferenced postings and recompute BM25 stats after per-file commits, so the
/// keyword lane's `N` / `avgdl` reflect this scan. Single-threaded; no-op without `code-search` or
/// when chunking is disabled. No-op on a `None` (read-only) index.
#[cfg(feature = "code-search")]
pub(crate) fn finalize_bm25_stats_if_any(store: &Store, config: &Config, sweep: bool) {
    if !crate::scanner_code::should_chunk(config) {
        return;
    }
    let Some(db) = store.index_db.as_ref() else {
        return;
    };
    let result = if sweep { db.sweep_bm25_postings() } else { Ok(()) };
    if let Err(error) = result.and_then(|()| db.recompute_bm25_stats()) {
        tracing::warn!(?error, "finalize BM25 index failed; keyword search may be stale");
    }
}

#[cfg(not(feature = "code-search"))]
pub(crate) fn finalize_bm25_stats_if_any(_store: &Store, _config: &Config, _sweep: bool) {}

#[cfg(feature = "code-search")]
pub(crate) fn bm25_posting_candidates<'a>(
    store: &Store,
    paths: impl IntoIterator<Item = &'a String>,
) -> ahash::AHashSet<Vec<u8>> {
    let Some(db) = store.index_db.as_ref() else {
        return ahash::AHashSet::new();
    };
    db.bm25_posting_candidates(paths).unwrap_or_else(|error| {
        tracing::warn!(%error, "collect BM25 retirement candidates failed; retry on full scan");
        ahash::AHashSet::new()
    })
}

#[cfg(feature = "code-search")]
pub(crate) fn retire_bm25_postings(store: &Store, candidates: ahash::AHashSet<Vec<u8>>) {
    if !candidates.is_empty()
        && let Some(db) = store.index_db.as_ref()
        && let Err(error) = db.retire_bm25_postings(candidates, |hash| store.hash_is_held(hash))
    {
        tracing::warn!(%error, "retire BM25 postings failed; retry on full scan");
    }
}

/// Compute the LanceDB scope key for this scan. Git sources reuse the existing remote-URL
/// scope derivation; the working-tree path falls back to a workdir-rooted key when there's
/// no git remote (or no git repo at all).
pub(crate) fn derive_scope(root: &Path, source: &crate::scanner::ScanSource<'_>) -> String {
    match source {
        crate::scanner::ScanSource::Staged(repo) | crate::scanner::ScanSource::Rev { repo, .. } => {
            crate::git::scope_key(repo)
        }
        crate::scanner::ScanSource::WorkingTree => match crate::git::Repo::discover(root) {
            Ok(repo) => crate::git::scope_key(&repo),
            Err(_) => format!("path:{}", root.display()),
        },
    }
}
