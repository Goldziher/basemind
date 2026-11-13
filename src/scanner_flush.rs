//! Post-drive flush helpers for [`crate::scanner`]: push buffered document and code-chunk batches
//! into LanceDB, purge removed files' rows, maintain the tables and refresh the BM25 stats. Each has
//! a no-op twin when its feature is compiled out.

use std::path::Path;

use crate::config::Config;
#[cfg(feature = "documents")]
use crate::scanner_docs::flush_document_batches;
use crate::scanner_drive::{PendingCodeBatchOpt, PendingDocBatchOpt};
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

/// Push the buffered document batches into LanceDB. No-op without the `documents` feature.
#[cfg(feature = "documents")]
pub(crate) fn flush_doc_batches_if_any(store: &mut Store, config: &Config, batches: Vec<PendingDocBatchOpt>) {
    if batches.is_empty() {
        return;
    }
    // Persist doc↔code links (ADR-0008) before the vector rows: `flush_document_batches` consumes
    // `batches`, and links are independent of embeddings (they persist even when embed is off).
    crate::scanner_doc_links::flush_doc_links(store, config, &batches);
    let _ = flush_document_batches(store, batches, &config.documents.embedding_preset);
}

#[cfg(not(feature = "documents"))]
pub(crate) fn flush_doc_batches_if_any(_store: &mut Store, _config: &Config, _batches: Vec<PendingDocBatchOpt>) {}

/// Purge `documents` LanceDB rows + `doc_files` entries for docs removed since the last scan. Called
/// after the batch flush so it reuses an already-open LanceStore. Only referenced under `documents`.
#[cfg(feature = "documents")]
pub(crate) fn flush_doc_removals_if_any(store: &mut Store, config: &Config, scope: &str, stale: &[String]) {
    crate::scanner_docs::delete_stale_documents(store, config, scope, stale);
}

/// Push the buffered code-chunk batches into LanceDB. No-op without the `code-search` feature.
#[cfg(feature = "code-search")]
pub(crate) fn flush_code_batches_if_any(
    store: &mut Store,
    config: &Config,
    scope: &str,
    batches: Vec<PendingCodeBatchOpt>,
) {
    if batches.is_empty() {
        return;
    }
    let _ = crate::scanner_code::flush_code_batches(store, scope, batches, &config.documents.embedding_preset);
}

#[cfg(not(feature = "code-search"))]
pub(crate) fn flush_code_batches_if_any(
    _store: &mut Store,
    _config: &Config,
    _scope: &str,
    _batches: Vec<PendingCodeBatchOpt>,
) {
}

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
