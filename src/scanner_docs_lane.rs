//! The scan's [`DocumentSink`]: [`DocumentLane`] extracts a document through xberg, persists its
//! blob and hands its rows to LanceDB, so the core scan reaches document extraction only through
//! the seam (ADR-0023 rule 1). The bodies are the document branch of `process_file` and the
//! document lanes of the post-drive flush.

#![cfg(feature = "documents")]

use std::path::Path;

use crate::config::{Config, DocumentsConfig};
use crate::extract::doc::DocBlobs;
use crate::hashing;
use crate::scan_retrieval::{DocumentSink, PendingDocBatch};
use crate::scanner::{EmbedMode, FileResult, FileStatus};
use crate::scanner_docs::{
    delete_stale_documents, doc_embed_requested, doc_entry_settled, doc_has_vectors_of, doc_scope_for,
    extract_and_persist_doc, flush_document_batches, pending_from_doc, purge_unembedded_documents,
    should_extract_document,
};
use crate::scanner_file::read_working_tree;
use crate::scanner_filter::Filters;
use crate::store::Store;

/// The document tier as the scan sees it.
pub(crate) struct DocumentLane;

impl DocumentSink for DocumentLane {
    fn warn_unavailable_settings(&self, cfg: &DocumentsConfig) {
        crate::extract::doc_tiers::warn_unavailable_settings(cfg);
    }

    fn process(
        &self,
        root: &Path,
        rel: &str,
        filters: &Filters,
        store: &Store,
        config: &Config,
        scope: &str,
        embed: EmbedMode,
    ) -> FileResult {
        process_doc(root, rel, filters, store, config, scope, embed)
    }

    fn abandoned_extractions(&self) -> usize {
        crate::extract::doc_guard::abandoned_extractions()
    }

    fn requeue(
        &self,
        store: &mut Store,
        config: &Config,
        filters: &Filters,
        scope: &str,
        batches: &mut Vec<PendingDocBatch>,
        budget: &mut usize,
    ) {
        requeue_pending_docs(store, config, filters, scope, batches, budget);
    }

    fn flush(&self, store: &mut Store, batches: Vec<PendingDocBatch>, model: &str) -> usize {
        flush_document_batches(store, batches, model)
    }

    fn remove(&self, store: &mut Store, config: &Config, scope: &str, stale: &[String]) {
        delete_stale_documents(store, config, scope, stale);
    }

    fn purge_unembedded(&self, store: &mut Store, config: &Config, filters: &Filters, scope: &str) -> bool {
        purge_unembedded_documents(store, config, filters, scope)
    }
}

/// Document-tier branch: file had no tree-sitter language; check `[documents]` config and
/// route through xberg. Always returns a `FileResult` — falls back to `SkippedNoLang`
/// when documents are disabled or the MIME type is filtered out.
fn process_doc(
    root: &Path,
    rel: &str,
    filters: &Filters,
    store: &Store,
    config: &Config,
    scope: &str,
    embed: EmbedMode,
) -> FileResult {
    let abs = root.join(rel);
    let effective_scope = doc_scope_for(rel, scope, config);
    let scope = effective_scope.as_ref();
    if !filters.doc_allowed(rel) {
        return FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang);
    }
    let Some(mime_type) = should_extract_document(&abs, &config.documents) else {
        return FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang);
    };
    let (bytes, size_bytes, mtime) =
        match read_working_tree(root, rel, filters.doc_max_file_bytes, config.scan.follow_symlinks) {
            Ok(triple) => triple,
            Err(status) => return FileResult::bare(rel.to_string(), status),
        };
    let hash = hashing::hash_bytes(&bytes);
    let hex_buf = hashing::hex_buf(&hash);
    let hash_hex = hashing::hex_str(&hex_buf);

    if let Some(existing) = store.lookup_doc(rel)
        && doc_entry_settled(
            existing,
            hash_hex,
            &config.documents.embedding_preset,
            doc_embed_requested(rel, &config.documents, embed, filters),
            &filters.doc_digest,
            filters.reflush_embeds,
        )
        && if filters.verify_blobs {
            store.doc_blob_is_intact(hash_hex)
        } else {
            store.blob_path_doc_hex(hash_hex).exists()
        }
    {
        return FileResult::bare(rel.to_string(), FileStatus::DocUnchanged);
    }

    // Timed out before on this exact content under these exact limits: a retry would only burn another
    // timeout (and strand another thread). Editing the file or changing a limit gets past this.
    if store.doc_tombstoned(rel, hash_hex, &filters.doc_guard_digest) {
        tracing::debug!(
            path = rel,
            "skipping document: it timed out before and nothing relevant changed"
        );
        return FileResult::bare(rel.to_string(), FileStatus::DocTombstoned);
    }

    // A document whose own estimated working set exceeds the whole ceiling can never fit: waiting ~keep
    // frees nothing and admitting it anyway is how the ceiling stopped meaning anything (a 7 MB ~keep
    // docx measured 12 GB). Skip it with a logged reason; this is a verdict on the file, not a ~keep
    // retry, and costs one header read. ~keep
    let gate = crate::backpressure::FootprintGate::new(config.resources.max_footprint_mb);
    let estimated = crate::extract::doc_cost::estimate_peak_bytes(&bytes, &mime_type);
    if gate.exceeds_budget(estimated) {
        tracing::warn!(
            path = rel,
            estimated_mb = estimated / (1024 * 1024),
            limit_mb = gate.limit_mb(),
            "skipping document: estimated extraction memory exceeds [resources] max_footprint_mb"
        );
        return FileResult::bare(rel.to_string(), FileStatus::SkippedTooLarge { size: size_bytes });
    }

    // Both bounds, in the order that makes them complementary: take a concurrency slot first so at ~keep
    // most `max_concurrent_documents` extractions can ever overlap, then serialise through the ~keep
    // footprint gate: over the ceiling only one document is admitted at a time (see ~keep
    // `FootprintGate::admit_exclusive`), so an overshoot is one document, not one per worker. ~keep
    let _doc_slot = crate::backpressure::acquire_doc_slot(config.resources.max_concurrent_documents);
    let admission = gate.admit_exclusive();
    tracing::debug!(path = rel, estimated_mb = estimated / (1024 * 1024), admitted = ?admission.outcome(), "extracting document");

    let doc_started = std::time::Instant::now();
    match extract_and_persist_doc(
        store,
        rel,
        &abs,
        &hash,
        &mime_type,
        &config.documents,
        &config.resources,
        filters,
        scope,
        embed,
    ) {
        Ok(Some(batch)) => {
            let status = FileStatus::DocIndexed {
                chunk_count: batch.chunk_count,
                embedding_dim: batch.embedding_dim,
                reused: batch.reused,
                degraded: batch.degraded,
                text_fallback: batch.text_fallback,
                elapsed_ms: u64::try_from(doc_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            };
            let doc_entry = crate::store::DocEntry {
                hash_hex: hash_hex.to_string(),
                embedding_preset: config.documents.embedding_preset.clone(),
                size_bytes,
                mtime,
                embedded: batch.embedded,
                embed_attempted: batch.embed_attempted,
                config_digest: filters.doc_digest.clone(),
            };
            let doc_upsert = match embed {
                EmbedMode::Inline => Some(doc_entry),
                EmbedMode::Deferred => None,
            };
            FileResult {
                path: rel.to_string(),
                status,
                upsert: None,
                doc_batch: Some(batch),
                doc_upsert,
                doc_tombstone: None,
                #[cfg(feature = "code-search")]
                code_batch: None,
            }
        }
        Ok(None) => FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang),
        Err(error) => {
            match error.downcast_ref::<crate::extract::ExtractError>() {
                Some(crate::extract::ExtractError::DocTimeout(budget)) => {
                    tracing::warn!(path = rel, "skipping document: {error:#}");
                    return FileResult {
                        doc_tombstone: Some(crate::store_doc_tombstone::DocTombstone {
                            hash_hex: hash_hex.to_string(),
                            guard_digest: filters.doc_guard_digest.clone(),
                            timeout_secs: budget.as_secs(),
                        }),
                        ..FileResult::bare(rel.to_string(), FileStatus::DocTimedOut)
                    };
                }
                Some(crate::extract::ExtractError::DocDeferred(_)) => {
                    tracing::debug!(path = rel, "deferring document: {error:#}");
                    return FileResult::bare(rel.to_string(), FileStatus::DocDeferred);
                }
                _ => {}
            }
            let msg = format!("document extract: {error:#}");
            if is_unsupported_format_error(&msg) {
                tracing::debug!(path = rel, reason = %msg, "skipping file: not an extractable document");
                FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang)
            } else {
                tracing::debug!(path = rel, error = %msg, "document extraction failed");
                FileResult::bare(rel.to_string(), FileStatus::ExtractFailed { msg })
            }
        }
    }
}

/// True when a document-extraction error means the file's format simply has no xberg
/// extractor (xberg's `UnsupportedFormat` → "Unsupported format: <mime>"), as opposed to a
/// genuine extraction failure on a real document. Such files are skipped, not failed.
fn is_unsupported_format_error(msg: &str) -> bool {
    msg.to_ascii_lowercase().contains("unsupported format")
}

/// Queue a batch for documents an earlier scan left owing rows and this scan did not process, at
/// most `budget` of them. The rows are rebuilt from the cached blob, so a path owes nothing, and
/// leaves the set, when no rows can come of it: it is no longer indexed, the embed policy or the
/// chunk cap now excludes it, its entry or its blob was embedded under another preset, or the blob
/// is gone or does not decode. Only a blob that could not be read from disk stays owed.
fn requeue_pending_docs(
    store: &mut Store,
    config: &Config,
    filters: &Filters,
    scope: &str,
    batches: &mut Vec<PendingDocBatch>,
    budget: &mut usize,
) {
    use crate::store::StoreError;

    if store.pending_vectors.docs().is_empty() {
        return;
    }
    let cfg = &config.documents;
    let preset = &cfg.embedding_preset;
    let queued: ahash::AHashSet<&str> = batches.iter().map(|batch| batch.rel_path.as_str()).collect();
    let mut requeued = Vec::new();
    let mut settled = Vec::new();
    for rel in store.pending_vectors.docs() {
        let rel_path = rel.to_str_lossy();
        if queued.contains(rel_path.as_ref()) {
            continue;
        }
        if *budget == 0 {
            break;
        }
        let wanted = cfg.enabled && cfg.embed && filters.doc_embed_allowed(&rel_path);
        let entry = store
            .lookup_doc(rel)
            .filter(|entry| wanted && entry.embedding_preset == *preset);
        let Some(entry) = entry else {
            settled.push(rel_path.into_owned());
            continue;
        };
        match store.read_doc_by_hex(&entry.hash_hex) {
            Ok(Some(doc)) if doc_has_vectors_of(&doc, preset) => {
                let doc_scope = doc_scope_for(&rel_path, scope, config);
                let batch = pending_from_doc(&doc, &rel_path, &entry.hash_hex, &doc_scope, cfg, true, true);
                if batch.emit_rows {
                    *budget -= 1;
                    requeued.push(batch);
                } else {
                    settled.push(rel_path.into_owned());
                }
            }
            Err(StoreError::Io { source, .. }) => {
                tracing::warn!(rel = %rel_path, error = %source, "doc blob unreadable; its vector rows stay owed");
            }
            Ok(_) | Err(_) => settled.push(rel_path.into_owned()),
        }
    }
    drop(queued);
    store.pending_vectors.settle_docs(&settled);
    batches.extend(requeued);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_format_error_is_a_skip_not_a_failure() {
        assert!(is_unsupported_format_error(
            "document extract: Unsupported format: application/x-wais-source"
        ));
        assert!(is_unsupported_format_error("Unsupported Format: text/x-foo"));
        assert!(!is_unsupported_format_error(
            "document extract: failed to parse PDF: corrupt xref table"
        ));
        assert!(!is_unsupported_format_error(
            "document extract: OCR engine returned no text"
        ));
    }
}

#[cfg(test)]
#[path = "scanner_flush_tests.rs"]
mod flush_tests;
