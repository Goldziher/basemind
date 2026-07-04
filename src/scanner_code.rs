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

use anyhow::{Context as _, Result};

use crate::chunk::{ChunkOptions, CodeChunkBlob, chunk_file};
use crate::config::Config;
use crate::embeddings::SharedEmbedder;
use crate::extract::{FileMapL1, FileMapL2, SCHEMA_VER};
use crate::lance::CodeRow;
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
}

/// Feature + config gate: chunk this scan only when `[code_search] enabled`.
pub(crate) fn should_chunk(config: &Config) -> bool {
    config.code_search.enabled
}

/// Chunk + embed one source file. Reuses the `.chunk` sidecar when an identical-content blob
/// already exists (the embedding cache); otherwise chunks, embeds in bulk, and writes the blob.
/// Returns `Ok(None)` when the file yields no chunks. Never opens LanceDB — the write is deferred.
pub(crate) fn chunk_and_embed(
    store: &Store,
    rel: &str,
    bytes: &[u8],
    l1: &FileMapL1,
    l2: Option<&FileMapL2>,
    hash_hex: &str,
    config: &Config,
) -> Result<Option<PendingCodeBatch>> {
    let cfg = &config.code_search;
    let opts = ChunkOptions {
        max_characters: cfg.max_characters,
        overlap: cfg.overlap,
    };
    // A stale-schema cached blob deserializes as an error (SchemaMismatch); treat any read
    // failure as a cache miss and recompute.
    let cached = store.read_chunks_by_hex(hash_hex).ok().flatten();

    if !cfg.embed {
        // Chunk-only: persist the sidecar (or reuse it) but emit no LanceDB rows.
        let chunks = match cached {
            Some(blob) => blob.chunks,
            None => {
                let chunks = chunk_file(rel, hash_hex, l1, l2, bytes, opts);
                let blob = CodeChunkBlob {
                    schema_ver: SCHEMA_VER,
                    embedding_dim: 0,
                    chunks: chunks.clone(),
                    embeddings: Vec::new(),
                };
                let _ = store.write_chunks_hex(hash_hex, &blob);
                chunks
            }
        };
        if chunks.is_empty() {
            return Ok(None);
        }
        return Ok(Some(PendingCodeBatch {
            rel_path: rel.to_string(),
            embedding_dim: 0,
            rows: Vec::new(),
        }));
    }

    // Embeddings on. `SharedEmbedder::load` is cheap (config only; the ONNX model is cached in
    // xberg and loaded lazily on the first `embed_batch`).
    let embedder = SharedEmbedder::load(&config.documents.embedding_preset).context("load code-search embedder")?;
    let dim = embedder.dim();

    // Cache hit: identical content already chunked + embedded at the current dim.
    if let Some(blob) = &cached
        && blob.embedding_dim == dim
        && !blob.chunks.is_empty()
        && blob.embeddings.len() == blob.chunks.len()
    {
        let rows = build_rows(rel, &blob.chunks, &blob.embeddings);
        return Ok(Some(PendingCodeBatch {
            rel_path: rel.to_string(),
            embedding_dim: dim,
            rows,
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
        let _ = store.write_chunks_hex(hash_hex, &blob);
        return Ok(None);
    }
    let texts: Vec<&str> = chunks.iter().map(|c| c.searchable_text.as_str()).collect();
    let embeddings = embedder
        .embed_batch(&texts)
        .with_context(|| format!("embed {} code chunks for {rel}", texts.len()))?;
    if embeddings.len() != chunks.len() {
        anyhow::bail!(
            "embedder returned {} vectors for {} chunks in {rel}",
            embeddings.len(),
            chunks.len()
        );
    }
    let blob = CodeChunkBlob {
        schema_ver: SCHEMA_VER,
        embedding_dim: dim,
        chunks: chunks.clone(),
        embeddings: embeddings.clone(),
    };
    // Best-effort: a blob-write failure only forfeits the embedding cache, not the scan.
    if let Err(error) = store.write_chunks_hex(hash_hex, &blob) {
        tracing::warn!(rel, ?error, "write code-chunk sidecar failed; embedding cache skipped");
    }
    let rows = build_rows(rel, &chunks, &embeddings);
    Ok(Some(PendingCodeBatch {
        rel_path: rel.to_string(),
        embedding_dim: dim,
        rows,
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
    match SharedEmbedder::load(embedding_model) {
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
