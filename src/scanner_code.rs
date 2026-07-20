//! Code-search branch helpers for the scanner (`code-search` feature).
//!
//! Mirrors `scanner_docs.rs` but for source files: after a file's L1/L2 filemap is written,
//! [`chunk_and_embed`] derives [`crate::chunk::CodeChunk`]s from the cached extraction + source
//! bytes (no re-parse), embeds each chunk's `searchable_text`, persists a content-addressed
//! `.chunk.msgpack` sidecar (the embedding cache), and hands back a [`PendingCodeBatch`] carrying
//! only lightweight metadata (path + blob hash + embedding dim) — never the LanceDB rows. The
//! single-threaded apply pass drains those descriptors and [`flush_code_batches`] re-reads each
//! file's sidecar to rebuild + write its rows one file at a time.
//!
//! The embed compute runs in the parallel per-file worker; the LanceDB write is deferred + serial
//! — exactly the document-tier pattern. Deferring the row-building (not just the write) keeps peak
//! embed-write RAM at one file's rows, independent of corpus size.

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

/// Per-file deferred-LanceDB-write descriptor for the code-search tier. Built inside the parallel
/// worker; consumed by [`flush_code_batches`] in the single-threaded apply pass.
///
/// Metadata only: it carries the content hash needed to re-read the persisted `.chunk.msgpack`
/// sidecar at flush time, NOT the `CodeRow`s or their embedding vectors. Holding the vectors here
/// (one `Vec<f32>` per chunk, across the whole corpus) was the memory leak this shape fixes.
///
/// The `bm25` postings are the exception — they are consumed by the worker itself (staged into
/// Fjall immediately after this descriptor is built) and cleared before the descriptor is
/// accumulated, so they never ride along the corpus-wide Vec either.
#[derive(Debug, Clone)]
pub(crate) struct PendingCodeBatch {
    /// Repository-relative path, forward-slash separated.
    pub rel_path: String,
    /// Content hash (hex) of the source file — the key under which the `.chunk.msgpack` sidecar is
    /// content-addressed. The flush re-reads that sidecar to rebuild rows on demand.
    pub blob_hash: String,
    /// Embedding vector length; `0` when embeddings were disabled (no rows to emit — BM25-only).
    pub embedding_dim: u16,
    /// BM25 keyword postings for this file's chunks — staged into the Fjall index by the worker
    /// (via [`crate::index::writer::IndexWriter::upsert_bm25_file`]), independent of embeddings, then
    /// cleared so the accumulated descriptor stays metadata-only.
    pub bm25: Vec<ChunkPosting>,
}

/// Feature + config gate: chunk this scan only when `[code_search] enabled`.
pub(crate) fn should_chunk(config: &Config) -> bool {
    config.code_search.enabled
}

/// A rows-empty [`PendingCodeBatch`] (`embedding_dim: 0`) carrying only the BM25 postings for
/// `chunks` — the graceful-degradation result when embeddings were requested but the embedder was
/// unavailable. `None` for a chunkless file (nothing to index).
fn bm25_batch_from_chunks(rel: &str, blob_hash: &str, chunks: &[crate::chunk::CodeChunk]) -> Option<PendingCodeBatch> {
    if chunks.is_empty() {
        return None;
    }
    Some(PendingCodeBatch {
        rel_path: rel.to_string(),
        blob_hash: blob_hash.to_string(),
        embedding_dim: 0,
        bm25: build_chunk_postings(chunks),
    })
}

/// True when a cached code-chunk blob can be reused as-is: identical content already chunked +
/// embedded under the current preset. Both the embedding **dimension** AND the **model/preset** must
/// match — `balanced` and `multilingual` share dim 768, so a dim-only check would falsely reuse
/// stale-model vectors across that switch — and the blob must actually carry one embedding per chunk.
/// Mirrors the documents tier's `scanner_docs::cached_doc_is_reusable`.
fn code_cache_is_reusable(blob: &CodeChunkBlob, dim: u16, preset: &str) -> bool {
    blob.embedding_dim == dim
        && blob.embedding_model == preset
        && !blob.chunks.is_empty()
        && blob.embeddings.len() == blob.chunks.len()
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
    let embed = matches!(mode, EmbedMode::Inline)
        && cfg.embed
        && !crate::scanner_filter::embed_excluded(rel, &cfg.embed_exclude);
    let opts = ChunkOptions {
        max_characters: cfg.max_characters,
        overlap: cfg.overlap,
    };
    let cached = store.read_chunks_by_hex(hash_hex).ok().flatten();

    if !embed {
        let chunks = match cached {
            Some(blob) => blob.chunks,
            None => {
                let chunks = chunk_file(rel, hash_hex, l1, l2, bytes, opts);
                let blob = CodeChunkBlob {
                    schema_ver: SCHEMA_VER,
                    embedding_dim: 0,
                    embedding_model: String::new(),
                    chunks: chunks.clone(),
                    embeddings: Vec::new(),
                };
                if let Err(error) = store.write_chunks_hex(hash_hex, &blob) {
                    tracing::warn!(rel, ?error, "write code-chunk sidecar (chunk-only) failed");
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
            blob_hash: hash_hex.to_string(),
            embedding_dim: 0,
            bm25,
        }));
    }

    let embedder = match SharedEmbedder::load(
        &config.documents.embedding_preset,
        config
            .resources
            .effective_embed_threads(config.documents.embed_max_threads),
        config.resources.embed_batch_size,
    ) {
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
            return Ok(bm25_batch_from_chunks(rel, hash_hex, &chunks));
        }
    };
    let dim = embedder.dim();

    if let Some(blob) = &cached
        && code_cache_is_reusable(blob, dim, &config.documents.embedding_preset)
    {
        let bm25 = build_chunk_postings(&blob.chunks);
        return Ok(Some(PendingCodeBatch {
            rel_path: rel.to_string(),
            blob_hash: hash_hex.to_string(),
            embedding_dim: dim,
            bm25,
        }));
    }

    let chunks = chunk_file(rel, hash_hex, l1, l2, bytes, opts);
    if chunks.is_empty() {
        let blob = CodeChunkBlob {
            schema_ver: SCHEMA_VER,
            embedding_dim: 0,
            embedding_model: String::new(),
            chunks: Vec::new(),
            embeddings: Vec::new(),
        };
        if let Err(error) = store.write_chunks_hex(hash_hex, &blob) {
            tracing::warn!(rel, ?error, "write empty code-chunk sidecar failed");
        }
        return Ok(None);
    }
    let texts: Vec<&str> = chunks.iter().map(|c| c.searchable_text.as_str()).collect();
    crate::backpressure::FootprintGate::new(config.resources.max_footprint_mb).admit();
    let embeddings = match embedder.embed_batch(&texts) {
        Ok(embeddings) if embeddings.len() == chunks.len() => embeddings,
        Ok(embeddings) => {
            tracing::warn!(
                rel,
                got = embeddings.len(),
                want = chunks.len(),
                "embedder returned wrong vector count; indexing BM25 keyword lane only"
            );
            return Ok(bm25_batch_from_chunks(rel, hash_hex, &chunks));
        }
        Err(error) => {
            tracing::warn!(rel, ?error, "embed code chunks failed; indexing BM25 keyword lane only");
            return Ok(bm25_batch_from_chunks(rel, hash_hex, &chunks));
        }
    };
    let bm25 = build_chunk_postings(&chunks);
    let blob = CodeChunkBlob {
        schema_ver: SCHEMA_VER,
        embedding_dim: dim,
        embedding_model: config.documents.embedding_preset.clone(),
        chunks,
        embeddings,
    };
    if let Err(error) = store.write_chunks_hex(hash_hex, &blob) {
        tracing::warn!(rel, ?error, "write code-chunk sidecar failed; embedding cache skipped");
    }
    Ok(Some(PendingCodeBatch {
        rel_path: rel.to_string(),
        blob_hash: hash_hex.to_string(),
        embedding_dim: dim,
        bm25,
    }))
}

/// Assemble the LanceDB rows for a file's chunks + embeddings (parallel arrays), **consuming** both
/// so the chunk text and embedding vectors move into the rows instead of being cloned. Called at
/// flush time with the owned sidecar blob; `scope` is stamped directly (no second pass needed).
fn build_rows_owned(
    rel: &str,
    scope: &str,
    chunks: Vec<crate::chunk::CodeChunk>,
    embeddings: Vec<Vec<f32>>,
) -> Vec<CodeRow> {
    chunks
        .into_iter()
        .zip(embeddings)
        .map(|(c, emb)| CodeRow {
            scope: scope.to_string(),
            path: rel.to_string(),
            chunk_id: c.chunk_id,
            symbol: c.symbol.unwrap_or_default(),
            kind: c.kind.unwrap_or_default(),
            lang: c.lang,
            line_start: c.line_start,
            line_end: c.line_end,
            byte_start: c.byte_start,
            byte_end: c.byte_end,
            text: c.text,
            embedding: emb,
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
    if store.lance.is_none() && !store.lance_dir_exists() {
        return;
    }
    let model = &config.documents.embedding_preset;
    let dim = match SharedEmbedder::load(model, 0, config.resources.embed_batch_size) {
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

/// Stream every pending code batch into the `code_chunks` LanceDB table, one file at a time. Opens
/// the store lazily — if no batch carries vectors (embeddings off) no LanceDB connection is made.
///
/// For each embedded file this re-reads the file's already-persisted `.chunk.msgpack` sidecar,
/// rebuilds its rows, writes them, and drops them before the next file — so peak embed-write RAM is
/// one file's rows plus one Arrow batch, independent of corpus size. (The sidecar is guaranteed on
/// disk here: every `embedding_dim > 0` batch either wrote it in `chunk_and_embed` or reused an
/// existing one, both before this post-barrier lane runs.)
///
/// Errors are logged and skipped per file so one malformed embedding never aborts the scan. Returns
/// the number of files for which rows were written.
pub(crate) fn flush_code_batches(
    store: &mut Store,
    scope: &str,
    batches: Vec<PendingCodeBatch>,
    embedding_model: &str,
) -> usize {
    let Some(dim) = batches.iter().find(|b| b.embedding_dim > 0).map(|b| b.embedding_dim) else {
        return 0;
    };
    match SharedEmbedder::load(
        embedding_model,
        0,
        crate::config::ResourcesConfig::default().embed_batch_size,
    ) {
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
    for batch in batches {
        if batch.embedding_dim == 0 {
            continue;
        }
        let blob = match store.read_chunks_by_hex(&batch.blob_hash) {
            Ok(Some(blob)) if blob.embedding_dim == dim && blob.embeddings.len() == blob.chunks.len() => blob,
            Ok(Some(_)) => {
                continue;
            }
            Ok(None) => {
                tracing::warn!(rel = %batch.rel_path, "chunk sidecar missing at flush; skipping vector rows");
                continue;
            }
            Err(error) => {
                tracing::warn!(rel = %batch.rel_path, ?error, "re-read chunk sidecar failed; skipping vector rows");
                continue;
            }
        };
        if blob.chunks.is_empty() {
            continue;
        }
        let rows = build_rows_owned(&batch.rel_path, scope, blob.chunks, blob.embeddings);
        match lance.replace_code_chunks(scope, &batch.rel_path, rows) {
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
        let batch = bm25_batch_from_chunks("src/lib.rs", "deadbeef", &[chunk("h:0", "alpha beta alpha")])
            .expect("non-empty chunks must yield a batch");
        assert_eq!(batch.rel_path, "src/lib.rs");
        assert_eq!(
            batch.blob_hash, "deadbeef",
            "carries the content hash for the flush re-read"
        );
        assert_eq!(batch.embedding_dim, 0, "no embeddings on the degraded path");
        assert_eq!(batch.bm25.len(), 1, "one posting per chunk");
        assert_eq!(batch.bm25[0].doclen, 3, "three tokens incl. the repeat");
    }

    #[test]
    fn bm25_batch_from_chunks_is_none_for_chunkless_file() {
        assert!(bm25_batch_from_chunks("src/empty.rs", "deadbeef", &[]).is_none());
    }

    /// Structural regression guard for the streaming-flush memory fix: the accumulated per-file
    /// descriptor must stay metadata-only. This exhaustive struct literal names EXACTLY the metadata
    /// fields — re-introducing a `rows: Vec<CodeRow>` (or any embedding payload) field would break
    /// this compile, catching a regression back to the corpus-wide accumulation that leaked GBs.
    #[test]
    fn pending_code_batch_is_metadata_only() {
        let batch = PendingCodeBatch {
            rel_path: "src/lib.rs".to_string(),
            blob_hash: "deadbeef".to_string(),
            embedding_dim: 768,
            bm25: Vec::new(),
        };
        assert_eq!(batch.embedding_dim, 768);
    }

    fn embedded_blob(model: &str, dim: u16) -> CodeChunkBlob {
        CodeChunkBlob {
            schema_ver: 0,
            embedding_dim: dim,
            embedding_model: model.to_string(),
            chunks: vec![chunk("h:0", "alpha")],
            embeddings: vec![vec![0.0_f32; dim as usize]],
        }
    }

    #[test]
    fn code_cache_reuse_requires_matching_dim_and_model() {
        let blob = embedded_blob("balanced", 768);
        assert!(code_cache_is_reusable(&blob, 768, "balanced"));
        assert!(
            !code_cache_is_reusable(&blob, 768, "multilingual"),
            "same-dim different-model must miss and force a re-embed"
        );
        assert!(!code_cache_is_reusable(&blob, 384, "balanced"));
        assert!(!code_cache_is_reusable(&embedded_blob("", 768), 768, "balanced"));
    }

    #[test]
    fn code_cache_reuse_rejects_chunkless_or_unembedded_blob() {
        let mut blob = embedded_blob("balanced", 768);
        blob.chunks.clear();
        blob.embeddings.clear();
        assert!(!code_cache_is_reusable(&blob, 768, "balanced"));
        let chunk_only = CodeChunkBlob {
            schema_ver: 0,
            embedding_dim: 768,
            embedding_model: "balanced".to_string(),
            chunks: vec![chunk("h:0", "alpha")],
            embeddings: Vec::new(),
        };
        assert!(!code_cache_is_reusable(&chunk_only, 768, "balanced"));
    }
}
