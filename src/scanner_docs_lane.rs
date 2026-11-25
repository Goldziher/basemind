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
    purge_unembedded_documents, should_extract_document_as,
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

    fn remove(&self, store: &mut Store, config: &Config, _filters: &Filters, stale: &[String]) {
        delete_stale_documents(store, config, stale);
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
    // A dependency's documentation (ADR-0049) is keyed by its virtual path `rel` but read from its
    // package directory, never through a symlink.
    let dep = filters.dep_doc(rel);
    let (root, file_rel) = match dep {
        Some(file) => (file.package_dir.as_path(), file.rel.as_str()),
        None => (root, rel),
    };
    let follow_symlinks = dep.is_none() && config.scan.follow_symlinks;
    let abs = root.join(file_rel);
    if !filters.doc_allowed(rel) {
        return FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang);
    }
    let Some(mime_type) = should_extract_document_as(&abs, &config.documents, dep.and_then(|file| file.mime)) else {
        return FileResult::bare(rel.to_string(), FileStatus::SkippedNoLang);
    };
    if let Some(size) = structured_data_over_chunk_cap(&abs, follow_symlinks, &mime_type, &config.documents) {
        return FileResult::bare(rel.to_string(), FileStatus::SkippedTooLarge { size });
    }
    let embed_requested = doc_embed_requested(rel, &config.documents, embed, filters);
    let unchanged = |existing: &crate::store::DocEntry, hash_hex: &str| {
        (doc_entry_settled(
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
    };
    // The code lane's `(size, mtime)` shortcut: an unchanged document is neither read nor hashed, so
    // a scan does not re-read every document in the tree. The recorded stamp is a settled one (0
    // when the read raced an edit), and 0 never matches. A document over `documents.max_file_bytes`
    // never takes it, so lowering the cap evicts an indexed one.
    if let Some(existing) = store.lookup_doc(rel)
        && existing.mtime != 0
        && let Ok(meta) = doc_stat(&abs, follow_symlinks)
        && meta.is_file()
        && meta.len() <= filters.doc_max_file_bytes
        && meta.len() == existing.size_bytes
        && crate::scanner_file::mtime_nanos(&meta) == existing.mtime
        && unchanged(existing, &existing.hash_hex)
    {
        return FileResult::bare(rel.to_string(), FileStatus::DocUnchanged);
    }

    // Taken before the read, not just around the extraction: a document is held in memory from here
    // until it is handed to xberg, so the slot bounds how many are held at once. ~keep
    let _doc_slot = crate::backpressure::acquire_doc_slot(config.resources.max_concurrent_documents);
    let (bytes, size_bytes, mtime) =
        match read_working_tree(root, file_rel, filters.doc_max_file_bytes, follow_symlinks) {
            Ok(triple) => triple,
            Err(status) => return FileResult::bare(rel.to_string(), status),
        };
    // The stamp the entry records: a stamp taken too close to the clock cannot vouch for the bytes
    // just read, so the shortcut above must not trust it (see `settled_mtime`). The raw stamp still
    // goes to the extraction, which checks the file did not change under it.
    let settled = crate::scanner_file::settled_mtime(mtime, crate::scanner_file::now_nanos());
    let hash = hashing::hash_bytes(&bytes);
    let hex_buf = hashing::hex_buf(&hash);
    let hash_hex = hashing::hex_str(&hex_buf);

    if let Some(existing) = store.lookup_doc(rel)
        && unchanged(existing, hash_hex)
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

    // Both bounds, in the order that makes them complementary: the concurrency slot taken before the ~keep
    // read means at most `max_concurrent_documents` extractions can ever overlap, then serialise ~keep
    // through the footprint gate: over the ceiling only one document is admitted at a time (see ~keep
    // `FootprintGate::admit_exclusive`), so an overshoot is one document, not one per worker. ~keep
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
                over_chunk_cap: batch.over_chunk_cap,
                elapsed_ms: u64::try_from(doc_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            };
            let doc_entry = crate::store::DocEntry {
                hash_hex: hash_hex.to_string(),
                embedding_preset: config.documents.embedding_preset.clone(),
                size_bytes,
                mtime: settled,
                embedded: batch.embedded,
                embed_attempted: batch.embed_attempted,
                config_digest: filters.doc_digest.clone(),
            };
            if matches!(embed, EmbedMode::Inline) && batch.embedding_dim > 0 {
                filters.docs_embedded.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let doc_upsert = match embed {
                EmbedMode::Inline => Some(doc_entry),
                EmbedMode::Deferred => deferred_doc_entry(rel, config, filters, &batch, doc_entry),
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

/// The entry a `Deferred` pass records. A document whose vectors are still to come is recorded as
/// owing them, so it is found by its text at the end of the fast pass and a later `Inline` pass
/// embeds it. Any other document (embedding is off for it, it has no chunks, it is over the chunk
/// cap, or its blob already carries vectors) owes none and is recorded as an `Inline` pass would
/// record it, so a repository that does not embed is still searchable after a fast pass.
fn deferred_doc_entry(
    rel: &str,
    config: &Config,
    filters: &Filters,
    batch: &PendingDocBatch,
    entry: crate::store::DocEntry,
) -> Option<crate::store::DocEntry> {
    let owes_vectors = config.documents.embed
        && filters.doc_embed_allowed(rel)
        && batch.embedding_dim == 0
        && batch.chunk_count > 0
        && !batch.over_chunk_cap;
    Some(if owes_vectors {
        crate::store::DocEntry {
            embedded: false,
            embed_attempted: false,
            ..entry
        }
    } else {
        entry
    })
}

/// The size of a structured-data document (JSON, YAML, XML, CSV and the like) that is certain to
/// pass `[documents] max_chunks_per_document`, read from its stat alone; `None` for every other
/// document. Such a file is machine-written bulk (a recorded API response, a generated fixture): it
/// would be read, chunked into thousands of pieces and keyword-indexed only to be left without
/// vectors, so the scan skips it before any of that. The linear chunker that cuts structured data
/// takes at most `max_characters` characters per chunk, so a file of `len` ASCII bytes (all
/// machine-written data in practice) yields at least `len / max_characters` chunks.
fn structured_data_over_chunk_cap(abs: &Path, follow_symlinks: bool, mime: &str, cfg: &DocumentsConfig) -> Option<u64> {
    if !crate::extract::doc_guard::is_structured_data(mime) {
        return None;
    }
    let size = doc_stat(abs, follow_symlinks)
        .ok()
        .filter(std::fs::Metadata::is_file)?
        .len();
    let least_chunks = size / u64::try_from(cfg.max_characters.max(1)).unwrap_or(u64::MAX);
    let cap = u64::try_from(cfg.max_chunks_per_document).unwrap_or(u64::MAX);
    (least_chunks > cap).then(|| {
        tracing::info!(
            path = %abs.display(),
            size,
            least_chunks,
            cap,
            "skipping structured data: it would pass max_chunks_per_document, so it is not chunked or indexed"
        );
        size
    })
}

/// The document's metadata as the read would see it: the link itself when links are not followed,
/// so a symlinked document never matches the shortcut (the read refuses it).
fn doc_stat(abs: &Path, follow_symlinks: bool) -> std::io::Result<std::fs::Metadata> {
    if follow_symlinks {
        std::fs::metadata(abs)
    } else {
        std::fs::symlink_metadata(abs)
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
    fn structured_data_certain_to_pass_the_chunk_cap_is_skipped_before_chunking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = DocumentsConfig {
            max_characters: 100,
            max_chunks_per_document: 3,
            ..DocumentsConfig::default()
        };
        let write = |name: &str, len: usize| {
            let path = dir.path().join(name);
            std::fs::write(&path, "a".repeat(len)).expect("write");
            path
        };
        let over = write("get_devices_valid.yaml", 401);
        let at_cap = write("small.yaml", 399);
        let prose = write("guide.md", 4_000);
        assert_eq!(
            structured_data_over_chunk_cap(&over, false, "application/yaml", &cfg),
            Some(401)
        );
        assert_eq!(
            structured_data_over_chunk_cap(&over, false, "application/json", &cfg),
            Some(401)
        );
        assert_eq!(
            structured_data_over_chunk_cap(&at_cap, false, "application/yaml", &cfg),
            None
        );
        assert_eq!(
            structured_data_over_chunk_cap(&prose, false, "text/markdown", &cfg),
            None,
            "prose over the cap is still indexed by keyword"
        );
        assert_eq!(
            structured_data_over_chunk_cap(&dir.path().join("missing.json"), false, "application/json", &cfg),
            None
        );
    }

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

#[cfg(test)]
#[path = "scanner_docs_deferral_tests.rs"]
mod deferral_tests;

#[cfg(all(test, unix))]
#[path = "scanner_docs_lane_failure_tests.rs"]
mod failure_tests;
