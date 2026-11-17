//! The scan's [`DocumentSink`]: [`DocumentLane`] extracts a document through xberg, persists its
//! blob and hands its rows to LanceDB, so the core scan reaches document extraction only through
//! the seam (ADR-0023 rule 1). The bodies are the document branch of `process_file` and the
//! document lanes of the post-drive flush.

#![cfg(feature = "documents")]

use std::path::Path;

use crate::config::{Config, DocumentsConfig};
use crate::hashing;
use crate::scan_retrieval::{DocumentSink, PendingDocBatch};
use crate::scanner::{EmbedMode, FileResult, FileStatus};
use crate::scanner_docs::{
    delete_stale_documents, doc_embed_requested, doc_entry_settled, extract_and_persist_doc, flush_document_batches,
    purge_unembedded_documents, should_extract_document,
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
        _scope: &str,
        embed: EmbedMode,
    ) -> FileResult {
        process_doc(root, rel, filters, store, config, embed)
    }

    fn embedder_ready(&self, config: &Config) -> bool {
        crate::extract::doc::text::document_embedder_ready(
            &config.documents.embedding_preset,
            config
                .resources
                .effective_embed_threads(config.documents.embed_max_threads),
            config.resources.embed_batch_size,
            config.resources.onnx_provider,
        )
    }

    fn abandoned_extractions(&self) -> usize {
        crate::extract::doc_guard::abandoned_extractions()
    }

    fn requeue(
        &self,
        store: &mut Store,
        config: &Config,
        filters: &Filters,
        _scope: &str,
        batches: &mut Vec<PendingDocBatch>,
        budget: &mut usize,
    ) {
        // The flush asks the store which documents it lacks and writes them, within the budget it
        // is handed, so nothing is queued here (ADR-0029 rule 11).
        let _ = (store, config, filters, batches, budget);
    }

    fn flush(
        &self,
        store: &mut Store,
        config: &Config,
        filters: &Filters,
        batches: Vec<PendingDocBatch>,
        budget: usize,
    ) -> usize {
        flush_document_batches(store, config, filters, batches, budget)
    }

    fn remove(&self, store: &mut Store, config: &Config, filters: &Filters, stale: &[String]) {
        delete_stale_documents(store, config, filters, stale);
    }

    fn purge_unembedded(&self, store: &mut Store, config: &Config, filters: &Filters, _scope: &str) -> bool {
        purge_unembedded_documents(store, config, filters)
    }
}

/// A document whose embedding failed owes its vectors: it is settled except for them. It is
/// re-extracted to pay them only once the embedder works (probed once per scan), so an offline
/// machine does not re-extract it on every scan.
fn owes_vectors_to_an_unavailable_embedder(
    existing: &crate::store::DocEntry,
    hash_hex: &str,
    embed_requested: bool,
    filters: &Filters,
    config: &Config,
) -> bool {
    embed_requested
        && !filters.reflush_embeds
        && doc_entry_settled(
            existing,
            hash_hex,
            &config.documents.embedding_preset,
            false,
            &filters.doc_digest,
            false,
        )
        && !filters.doc_embedder_ready(config)
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
    embed: EmbedMode,
) -> FileResult {
    let abs = root.join(rel);
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

    let embed_requested = doc_embed_requested(rel, &config.documents, embed, filters);
    if let Some(existing) = store.lookup_doc(rel)
        && (doc_entry_settled(
            existing,
            hash_hex,
            &config.documents.embedding_preset,
            embed_requested,
            &filters.doc_digest,
            filters.reflush_embeds,
        ) || owes_vectors_to_an_unavailable_embedder(existing, hash_hex, embed_requested, filters, config))
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
    // xberg reads the file itself; holding this copy through the extraction would double the
    // document's footprint for nothing.
    drop(bytes);

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
        (size_bytes, mtime),
        &mime_type,
        &config.documents,
        &config.resources,
        filters,
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
