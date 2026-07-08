//! Code-search branch helpers for the scanner (`code-search` feature).
//!
//! Mirrors `scanner_docs.rs` but for source files: after a file's L1/L2 filemap is written,
//! [`chunk_and_embed`] derives [`crate::chunk::CodeChunk`]s from the cached extraction + source
//! bytes (no re-parse), embeds each chunk's `searchable_text`, persists a content-addressed
//! `.chunk.msgpack` sidecar (the embedding cache), and hands back a [`PendingCodeBatch`] carrying
//! the LanceDB rows. The single-threaded apply pass drains those batches and
//! [`flush_code_batches`] pushes them into the `code_chunks` table.
//!
//! The embed compute runs in the parallel per-file worker; the LanceDB write is deferred + serial
//! — exactly the document-tier pattern.

#![cfg(feature = "code-search")]

use anyhow::Result;

use crate::chunk::{ChunkOptions, CodeChunkBlob, chunk_file};
use crate::config::Config;
use crate::embeddings::SharedEmbedder;
use crate::extract::{FileMapL1, FileMapL2, SCHEMA_VER};
use crate::lance::CodeRow;
use crate::scanner::EmbedMode;
use crate::search::bm25::{ChunkPosting, build_chunk_postings};
use crate::store::Store;

/// Per-file deferred LanceDB write for the code-search tier. Built inside the parallel worker;
/// consumed by [`flush_code_batches`] in the single-threaded apply pass.
#[derive(Debug, Clone)]
pub(crate) struct PendingCodeBatch {
    /// Repository-relative path, forward-slash separated.
    pub rel_path: String,
    /// Embedding vector length; `0` when embeddings were disabled.
    pub embedding_dim: u16,
    /// The rows ready to land in the `code_chunks` table. Empty when embeddings were disabled.
    /// The `scope` field on each row is filled in by [`flush_code_batches`] at write time.
    pub rows: Vec<CodeRow>,
    /// BM25 keyword postings for this file's chunks — staged into the Fjall index by the worker
    /// (via [`crate::index::writer::IndexWriter::upsert_bm25_file`]), independent of embeddings.
    pub bm25: Vec<ChunkPosting>,
}

/// Feature + config gate: chunk this scan only when `[code_search] enabled`.
pub(crate) fn should_chunk(config: &Config) -> bool {
    config.code_search.enabled
}

/// A rows-empty [`PendingCodeBatch`] carrying only the BM25 postings for `chunks` — the
/// graceful-degradation result when embeddings were requested but the embedder was unavailable.
/// `None` for a chunkless file (nothing to index).
fn bm25_batch_from_chunks(rel: &str, chunks: &[crate::chunk::CodeChunk]) -> Option<PendingCodeBatch> {
    if chunks.is_empty() {
        return None;
    }
    Some(PendingCodeBatch {
        rel_path: rel.to_string(),
        embedding_dim: 0,
        rows: Vec::new(),
        bm25: build_chunk_postings(chunks),
    })
}

/// Chunk + embed one source file. Reuses the `.chunk` sidecar when an identical-content blob
/// already exists (the embedding cache); otherwise chunks, embeds in bulk, and writes the blob.
/// Returns `Ok(None)` when the file yields no chunks. Never opens LanceDB — the write is deferred.
#[allow(clippy::too_many_arguments)]
pub(crate) fn chunk_and_embed(
    store: &Store,
    rel: &str,
    bytes: &[u8],
    l1: &FileMapL1,
    l2: Option<&FileMapL2>,
    hash_hex: &str,
    config: &Config,
    mode: EmbedMode,
) -> Result<Option<PendingCodeBatch>> {
    let cfg = &config.code_search;
    // `Deferred` forces the chunk-only path regardless of the configured `code_search.embed`, so
    // serve boot writes symbols + BM25 fast and leaves embeddings to the background `Inline` pass.
    // A file matching `code_search.embed_exclude` is still chunked + BM25-indexed below, only its
    // embedding is skipped (the `!embed` branch persists the vector-less sidecar).
    let embed = matches!(mode, EmbedMode::Inline)
        && cfg.embed
        && !crate::scanner_filter::embed_excluded(rel, &cfg.embed_exclude);
    let opts = ChunkOptions {
        max_characters: cfg.max_characters,
        overlap: cfg.overlap,
    };
    // A stale-schema cached blob deserializes as an error (SchemaMismatch); treat any read
    // failure as a cache miss and recompute.
    let cached = store.read_chunks_by_hex(hash_hex).ok().flatten();

    if !embed {
        // Chunk-only: persist the sidecar (or reuse it) but emit no LanceDB rows.
        let chunks = match cached {
            Some(blob) => blob.chunks,
            None => {
                let chunks = chunk_file(rel, hash_hex, l1, l2, bytes, opts);
                // Only a genuine `code_search.embed = false` (an `Inline` chunk-only scan) persists
                // the sidecar. In `Deferred` mode we must NOT: the sidecar's presence is the
                // "already processed" signal `process_file`'s unchanged-skip keys on, so writing a
                // vector-less one here would make the background `Inline` fill-in pass skip the file
                // and never embed it. Leaving it absent lets that pass re-chunk + embed.
                if matches!(mode, EmbedMode::Inline) {
                    let blob = CodeChunkBlob {
                        schema_ver: SCHEMA_VER,
                        embedding_dim: 0,
                        chunks: chunks.clone(),
                        embeddings: Vec::new(),
                    };
                    if let Err(error) = store.write_chunks_hex(hash_hex, &blob) {
                        tracing::warn!(rel, ?error, "write code-chunk sidecar (chunk-only) failed");
                    }
                }
                chunks
            }
        };
        if chunks.is_empty() {
            return Ok(None);
        }
        let bm25 = build_chunk_postings(&chunks);
        return Ok(Some(PendingCodeBatch {
            rel_path: rel.to_string(),
            embedding_dim: 0,
            rows: Vec::new(),
            bm25,
        }));
    }

    // Embeddings on. `SharedEmbedder::load` is cheap (config only; the ONNX model is cached in
    // xberg and loaded lazily on the first `embed_batch`).
    //
    // BM25 is independent of embeddings: if the embedder cannot load or run, we still want the
    // keyword lane populated, so every embed-failure path below degrades to a rows-empty BM25 batch
    // (via [`bm25_batch_from_chunks`]) rather than propagating an error that would drop the file's
    // postings. No embedded sidecar is written on failure, so the next scan retries embedding.
    let embedder = match SharedEmbedder::load(&config.documents.embedding_preset, config.documents.embed_max_threads) {
        Ok(embedder) => embedder,
        Err(error) => {
            tracing::warn!(
                rel,
                ?error,
                "load code-search embedder failed; indexing BM25 keyword lane only"
            );
            let chunks = match cached {
                Some(blob) if !blob.chunks.is_empty() => blob.chunks,
                _ => chunk_file(rel, hash_hex, l1, l2, bytes, opts),
            };
            return Ok(bm25_batch_from_chunks(rel, &chunks));
        }
    };
    let dim = embedder.dim();

    // Cache hit: identical content already chunked + embedded at the current dim.
    if let Some(blob) = &cached
        && blob.embedding_dim == dim
        && !blob.chunks.is_empty()
        && blob.embeddings.len() == blob.chunks.len()
    {
        let rows = build_rows(rel, &blob.chunks, &blob.embeddings);
        let bm25 = build_chunk_postings(&blob.chunks);
        return Ok(Some(PendingCodeBatch {
            rel_path: rel.to_string(),
            embedding_dim: dim,
            rows,
            bm25,
        }));
    }

    // Compute from scratch.
    let chunks = chunk_file(rel, hash_hex, l1, l2, bytes, opts);
    if chunks.is_empty() {
        // Persist an empty sidecar so an unchanged empty-of-chunks file is skipped next scan.
        let blob = CodeChunkBlob {
            schema_ver: SCHEMA_VER,
            embedding_dim: 0,
            chunks: Vec::new(),
            embeddings: Vec::new(),
        };
        if let Err(error) = store.write_chunks_hex(hash_hex, &blob) {
            tracing::warn!(rel, ?error, "write empty code-chunk sidecar failed");
        }
        return Ok(None);
    }
    let texts: Vec<&str> = chunks.iter().map(|c| c.searchable_text.as_str()).collect();
    let embeddings = match embedder.embed_batch(&texts) {
        Ok(embeddings) if embeddings.len() == chunks.len() => embeddings,
        Ok(embeddings) => {
            tracing::warn!(
                rel,
                got = embeddings.len(),
                want = chunks.len(),
                "embedder returned wrong vector count; indexing BM25 keyword lane only"
            );
            return Ok(bm25_batch_from_chunks(rel, &chunks));
        }
        Err(error) => {
            tracing::warn!(rel, ?error, "embed code chunks failed; indexing BM25 keyword lane only");
            return Ok(bm25_batch_from_chunks(rel, &chunks));
        }
    };
    // Build the deferred LanceDB rows + BM25 postings while borrowing `chunks` + `embeddings`, then
    // MOVE both into the sidecar blob — no clone of the per-chunk `String` fields on the hot path.
    let rows = build_rows(rel, &chunks, &embeddings);
    let bm25 = build_chunk_postings(&chunks);
    let blob = CodeChunkBlob {
        schema_ver: SCHEMA_VER,
        embedding_dim: dim,
        chunks,
        embeddings,
    };
    // Best-effort: a blob-write failure only forfeits the embedding cache, not the scan.
    if let Err(error) = store.write_chunks_hex(hash_hex, &blob) {
        tracing::warn!(rel, ?error, "write code-chunk sidecar failed; embedding cache skipped");
    }
    Ok(Some(PendingCodeBatch {
        rel_path: rel.to_string(),
        embedding_dim: dim,
        rows,
        bm25,
    }))
}

/// Assemble the LanceDB rows for a file's chunks + embeddings (parallel arrays). `scope` is left
/// empty here and stamped by [`flush_code_batches`] at write time (it is a flush-time concern).
fn build_rows(rel: &str, chunks: &[crate::chunk::CodeChunk], embeddings: &[Vec<f32>]) -> Vec<CodeRow> {
    chunks
        .iter()
        .zip(embeddings.iter())
        .map(|(c, emb)| CodeRow {
            scope: String::new(),
            path: rel.to_string(),
            chunk_id: c.chunk_id.clone(),
            symbol: c.symbol.clone().unwrap_or_default(),
            kind: c.kind.clone().unwrap_or_default(),
            lang: c.lang.clone(),
            line_start: c.line_start,
            line_end: c.line_end,
            byte_start: c.byte_start,
            byte_end: c.byte_end,
            text: c.text.clone(),
            embedding: emb.clone(),
        })
        .collect()
}

/// Delete the `code_chunks` rows of files that no longer exist (or are no longer allowed). Runs in
/// the serial apply pass after the batch flush, so it reuses an already-open LanceStore when the
/// same scan wrote chunks. Best-effort: never opens (i.e. never *creates*) the vector store when it
/// does not already exist, and logs — never propagates — a delete failure.
pub(crate) fn delete_stale_code_chunks(store: &mut Store, config: &Config, scope: &str, stale: &[String]) {
    if stale.is_empty() || !should_chunk(config) {
        return;
    }
    // Nothing to purge if the vector store was never built for this repo — do not create it here.
    if store.lance.is_none() && !store.lance_dir_exists() {
        return;
    }
    let model = &config.documents.embedding_preset;
    // The store's `(dim, model)` are fixed at creation; `lance_or_open` validates the pair. Deriving
    // the dim from the preset lets us open the existing store even on a delete-only rescan (no
    // batches to read a dim from).
    let dim = match SharedEmbedder::load(model, 0) {
        Ok(embedder) => embedder.dim(),
        Err(error) => {
            tracing::warn!(?error, preset = %model, "code-chunk stale purge: unknown embedding preset; skipping");
            return;
        }
    };
    let lance = match store.lance_or_open(dim, model) {
        Ok(lance) => lance.clone(),
        Err(error) => {
            tracing::warn!(?error, "code-chunk stale purge: open LanceStore failed; skipping");
            return;
        }
    };
    for path in stale {
        if let Err(error) = lance.delete_code_chunks(scope, path) {
            tracing::warn!(
                rel = %path,
                ?error,
                "code-chunk stale purge failed; search_code may return a removed path"
            );
        }
    }
}

/// Push every pending code batch into the `code_chunks` LanceDB table. Opens the store lazily —
/// if every batch is empty (embeddings off) no LanceDB connection is made. Errors are logged and
/// skipped per file so one malformed embedding never aborts the scan. Returns the number of files
/// for which rows were written.
pub(crate) fn flush_code_batches(
    store: &mut Store,
    scope: &str,
    batches: Vec<PendingCodeBatch>,
    embedding_model: &str,
) -> usize {
    let Some(dim) = batches.iter().find(|b| b.embedding_dim > 0).map(|b| b.embedding_dim) else {
        return 0;
    };
    // Validate the configured preset against the runtime dim before writing (mirrors the
    // document tier's guard) — a mismatch means an unknown preset or a swapped backend.
    match SharedEmbedder::load(embedding_model, 0) {
        Ok(embedder) if embedder.dim() != dim => {
            tracing::error!(
                preset = %embedding_model,
                expected = embedder.dim(),
                actual = dim,
                "preset/runtime dim mismatch — refusing to write code_chunks batch"
            );
            return 0;
        }
        Ok(_) => {}
        Err(error) => {
            tracing::error!(?error, preset = %embedding_model, "unknown embedding preset — refusing to write code_chunks batch");
            return 0;
        }
    }

    let lance = match store.lance_or_open(dim, embedding_model) {
        Ok(s) => s.clone(),
        Err(error) => {
            tracing::error!(?error, "open LanceStore for code_chunks batch failed");
            return 0;
        }
    };

    let mut inserted = 0usize;
    for mut batch in batches {
        if batch.rows.is_empty() {
            continue;
        }
        // Stamp the scan-wide scope onto each row here (build time left it empty).
        for row in &mut batch.rows {
            row.scope = scope.to_string();
        }
        match lance.replace_code_chunks(scope, &batch.rel_path, batch.rows) {
            Ok(()) => inserted += 1,
            Err(error) => {
                tracing::warn!(rel = %batch.rel_path, ?error, "lance replace_code_chunks failed; code search may be incomplete");
            }
        }
    }
    inserted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::CodeChunk;

    fn chunk(chunk_id: &str, searchable_text: &str) -> CodeChunk {
        CodeChunk {
            chunk_id: chunk_id.to_string(),
            path: "src/lib.rs".to_string(),
            lang: "rust".to_string(),
            kind: None,
            symbol: None,
            signature: None,
            doc: None,
            byte_start: 0,
            byte_end: 0,
            line_start: 1,
            line_end: 1,
            text: searchable_text.to_string(),
            searchable_text: searchable_text.to_string(),
        }
    }

    #[test]
    fn bm25_batch_from_chunks_is_rows_empty_with_postings() {
        // The embed-failure degradation: BM25 postings survive, no LanceDB rows, dim 0.
        let batch = bm25_batch_from_chunks("src/lib.rs", &[chunk("h:0", "alpha beta alpha")])
            .expect("non-empty chunks must yield a batch");
        assert_eq!(batch.rel_path, "src/lib.rs");
        assert_eq!(batch.embedding_dim, 0, "no embeddings on the degraded path");
        assert!(batch.rows.is_empty(), "no LanceDB rows without embeddings");
        assert_eq!(batch.bm25.len(), 1, "one posting per chunk");
        assert_eq!(batch.bm25[0].doclen, 3, "three tokens incl. the repeat");
    }

    #[test]
    fn bm25_batch_from_chunks_is_none_for_chunkless_file() {
        assert!(bm25_batch_from_chunks("src/empty.rs", &[]).is_none());
    }
}
