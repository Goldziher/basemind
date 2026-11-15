//! Post-drive flush helpers for [`crate::scanner`]: push buffered document and code-chunk batches
//! into LanceDB, purge removed files' rows, maintain the tables and refresh the BM25 stats. Each has
//! a no-op twin when its feature is compiled out.

use std::path::Path;

use crate::config::Config;
#[cfg(feature = "documents")]
use crate::scanner_docs::flush_document_batches;
use crate::scanner_drive::{PendingCodeBatchOpt, PendingDocBatchOpt};
use crate::scanner_lanes::{LANE_CODE_BATCHES, LANE_DOC_BATCHES, run_optional_lane};
use crate::store::Store;

/// Compact fragments and prune old table versions of the LanceDB store once this scan's writes are
/// done (#76). No-op when the scan never opened the store or wrote nothing.
pub(crate) fn maintain_lance_if_open(store: &Store) {
    #[cfg(any(feature = "documents", feature = "code-search"))]
    if let Some(lance) = store.lance.as_ref()
        && let Err(error) = lance.maintain(crate::lance::SCAN_RETENTION)
    {
        tracing::warn!(?error, "lance post-scan maintenance failed; old table versions kept");
    }
    #[cfg(not(any(feature = "documents", feature = "code-search")))]
    let _ = store;
}

/// Write the vector rows this scan owes, and the ones an earlier scan left owing, then drop the
/// settled paths from the file map with a second flush (see [`crate::store_pending`]).
///
/// The lanes are fault-contained and report nothing: what they could not write simply stays in
/// `pending_vectors`. Only the flush can fail.
pub(crate) fn flush_vector_lanes(
    store: &mut Store,
    config: &Config,
    scope: &str,
    mut doc_batches: Vec<PendingDocBatchOpt>,
    mut code_batches: Vec<PendingCodeBatchOpt>,
) -> Result<(), crate::store::StoreError> {
    let owed = store.index.pending_vectors.len();
    requeue_pending_docs(store, config, scope, &mut doc_batches);
    requeue_pending_code(store, config, &mut code_batches);
    run_optional_lane(LANE_DOC_BATCHES, || {
        flush_doc_batches_if_any(store, config, doc_batches);
    });
    run_optional_lane(LANE_CODE_BATCHES, || {
        flush_code_batches_if_any(store, config, scope, code_batches);
    });
    // The lanes only ever remove paths, so an unchanged count is an unchanged set.
    if store.index.pending_vectors.len() != owed {
        store.flush()?;
    }
    Ok(())
}

/// Queue a batch for every document an earlier scan left owing rows and this scan did not process.
/// Its rows are rebuilt from the cached blob. A path that is no longer indexed, or whose entry was
/// embedded under another preset (the scan re-embeds it and queues it again), owes nothing.
#[cfg(feature = "documents")]
fn requeue_pending_docs(store: &mut Store, config: &Config, scope: &str, batches: &mut Vec<PendingDocBatchOpt>) {
    if store.index.pending_vectors.docs.is_empty() {
        return;
    }
    let preset = &config.documents.embedding_preset;
    let Ok(dim) = crate::scanner_docs::preset_dim(preset) else {
        return;
    };
    let queued: ahash::AHashSet<&str> = batches.iter().map(|batch| batch.rel_path.as_str()).collect();
    let mut requeued = Vec::new();
    let mut settled = Vec::new();
    for rel in &store.index.pending_vectors.docs {
        let rel_path = rel.to_str_lossy();
        if queued.contains(rel_path.as_ref()) {
            continue;
        }
        match store.lookup_doc(rel) {
            Some(entry) if entry.embedding_preset == *preset => {
                let doc_scope = crate::scanner_docs::doc_scope_for(&rel_path, scope, config).into_owned();
                requeued.push(crate::scanner_docs::PendingDocBatch {
                    blob_hash: entry.hash_hex.clone(),
                    doc_scope,
                    chunk_count: 0,
                    embedding_dim: dim,
                    emit_rows: true,
                    embedded: entry.embedded,
                    embed_attempted: entry.embed_attempted,
                    reused: true,
                    degraded: false,
                    text_fallback: false,
                    rel_path: rel_path.into_owned(),
                });
            }
            _ => settled.push(rel_path.into_owned()),
        }
    }
    drop(queued);
    store.index.pending_vectors.settle_docs(&settled);
    batches.extend(requeued);
}

#[cfg(not(feature = "documents"))]
fn requeue_pending_docs(_store: &mut Store, _config: &Config, _scope: &str, _batches: &mut Vec<PendingDocBatchOpt>) {}

/// Queue a batch for every code file an earlier scan left owing rows and this scan did not process.
/// Its rows are rebuilt from the cached chunk sidecar. A path that is no longer indexed, or whose
/// sidecar carries no vectors of the current preset (the scan re-embeds it and queues it again),
/// owes nothing; one whose sidecar cannot be read stays owed.
#[cfg(feature = "code-search")]
fn requeue_pending_code(store: &mut Store, config: &Config, batches: &mut Vec<PendingCodeBatchOpt>) {
    if store.index.pending_vectors.code.is_empty() {
        return;
    }
    let queued: ahash::AHashSet<&str> = batches.iter().map(|batch| batch.rel_path.as_str()).collect();
    let mut requeued = Vec::new();
    let mut settled = Vec::new();
    for rel in &store.index.pending_vectors.code {
        let rel_path = rel.to_str_lossy();
        if queued.contains(rel_path.as_ref()) {
            continue;
        }
        let Some(entry) = store.lookup(rel) else {
            settled.push(rel_path.into_owned());
            continue;
        };
        match store.peek_chunk_state(&entry.hash_hex) {
            Ok(Some(peek)) if peek.embedding_dim > 0 && peek.embedding_model == config.documents.embedding_preset => {
                requeued.push(crate::scanner_code::PendingCodeBatch {
                    rel_path: rel_path.into_owned(),
                    blob_hash: entry.hash_hex.clone(),
                    embedding_dim: peek.embedding_dim,
                    bm25: Vec::new(),
                });
            }
            Ok(_) => settled.push(rel_path.into_owned()),
            Err(error) => {
                tracing::warn!(rel = %rel_path, %error, "chunk sidecar unreadable; its vector rows stay owed");
            }
        }
    }
    drop(queued);
    store.index.pending_vectors.settle_code(&settled);
    batches.extend(requeued);
}

#[cfg(not(feature = "code-search"))]
fn requeue_pending_code(_store: &mut Store, _config: &Config, _batches: &mut Vec<PendingCodeBatchOpt>) {}

/// Push the buffered document batches into LanceDB. No-op without the `documents` feature.
#[cfg(feature = "documents")]
fn flush_doc_batches_if_any(store: &mut Store, config: &Config, batches: Vec<PendingDocBatchOpt>) {
    if batches.is_empty() {
        return;
    }
    let _ = flush_document_batches(store, batches, &config.documents.embedding_preset);
}

#[cfg(not(feature = "documents"))]
fn flush_doc_batches_if_any(_store: &mut Store, _config: &Config, _batches: Vec<PendingDocBatchOpt>) {}

/// Purge `documents` LanceDB rows + `doc_files` entries for docs removed since the last scan. Called
/// after the batch flush so it reuses an already-open LanceStore. Only referenced under `documents`.
#[cfg(feature = "documents")]
pub(crate) fn flush_doc_removals_if_any(store: &mut Store, config: &Config, scope: &str, stale: &[String]) {
    crate::scanner_docs::delete_stale_documents(store, config, scope, stale);
}

/// Push the buffered code-chunk batches into LanceDB. No-op without the `code-search` feature.
#[cfg(feature = "code-search")]
fn flush_code_batches_if_any(store: &mut Store, config: &Config, scope: &str, batches: Vec<PendingCodeBatchOpt>) {
    if batches.is_empty() {
        return;
    }
    let _ = crate::scanner_code::flush_code_batches(store, scope, batches, &config.documents.embedding_preset);
}

#[cfg(not(feature = "code-search"))]
fn flush_code_batches_if_any(_store: &mut Store, _config: &Config, _scope: &str, _batches: Vec<PendingCodeBatchOpt>) {}

/// Purge `code_chunks` rows for files removed since the last scan. No-op without `code-search`.
/// Called after the batch flush so it reuses an already-open LanceStore.
#[cfg(feature = "code-search")]
pub(crate) fn flush_code_removals_if_any(store: &mut Store, config: &Config, scope: &str, stale: &[String]) {
    crate::scanner_code::delete_stale_code_chunks(store, config, scope, stale);
}

#[cfg(not(feature = "code-search"))]
pub(crate) fn flush_code_removals_if_any(_store: &mut Store, _config: &Config, _scope: &str, _stale: &[String]) {}

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
