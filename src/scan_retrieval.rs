//! The traits through which the scan reaches the document and vector stack (ADR-0023 rule 1): an
//! `Embedder` for code chunks, a `VectorStore` the chunk rows are written to, and a
//! `DocumentSink` that extracts, embeds and stores documents. The scan names no LanceDB, embedding
//! or extraction type; the implementations live in the retrieval modules and are bound once per
//! [`Filters`](crate::scanner_filter::Filters), so a scan resolves them once, never per file.
//!
//! The traits are synchronous and carry no runtime type: whatever the retrieval side runs on (its
//! own tokio runtime) stays behind them.

#![cfg(any(feature = "documents", feature = "code-search"))]

use std::path::Path;

use crate::config::Config;
#[cfg(feature = "documents")]
use crate::config::DocumentsConfig;
#[cfg(feature = "code-search")]
use crate::config::OnnxProvider;
#[cfg(feature = "documents")]
use crate::scanner::{EmbedMode, FileResult};
#[cfg(feature = "code-search")]
use crate::scanner_code::PendingCodeBatch;
#[cfg(feature = "documents")]
use crate::scanner_filter::Filters;
use crate::store::Store;

/// A failure of the retrieval side, as text: the scan only logs it. Built from an `anyhow` error
/// with `{:#}`, so the text carries the whole cause chain; log it with `%`.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RetrievalError(pub String);

/// Which embedding model to run, and how.
#[cfg(feature = "code-search")]
#[derive(Debug, Clone, Copy)]
pub struct EmbedderSpec<'a> {
    /// The embedding preset (`[documents] embedding_preset`).
    pub preset: &'a str,
    /// Cap on the embedding pool's threads; `0` picks the default.
    pub max_threads: usize,
    /// Texts per model call.
    pub batch_size: usize,
    /// ONNX execution provider.
    pub provider: OnnxProvider,
}

/// Turns chunk text into vectors.
#[cfg(feature = "code-search")]
pub trait Embedder: Sync {
    /// Vector length of `spec`'s model. Fails for an unknown preset, exactly as loading it would.
    fn dim(&self, spec: &EmbedderSpec<'_>) -> Result<u16, RetrievalError>;

    /// One vector per text, in order.
    fn embed_batch(&self, spec: &EmbedderSpec<'_>, texts: &[&str]) -> Result<Vec<Vec<f32>>, RetrievalError>;
}

/// The vector store a scan writes chunk rows to. Each method logs its own failures where the
/// scan has nothing to do with them.
pub trait VectorStore: Sync {
    /// Open the store for the configured preset (creating it when absent). An error means no row
    /// can be written this scan.
    fn open(&self, store: &mut Store, config: &Config) -> Result<(), RetrievalError>;

    /// Id of the vector store on disk at `dir`, `None` when there is none or it is unreadable.
    fn store_id_at(&self, dir: &Path) -> Option<String>;

    /// Id of the vector store this `store` has open, if any.
    fn open_store_id(&self, store: &Store) -> Option<String>;

    /// Drop the handle `store` keeps, so the next write reopens.
    fn close(&self, store: &mut Store);

    /// Compact and prune the store after a scan's writes; no-op when the scan never opened it.
    fn maintain(&self, store: &Store);

    /// Publish this scan's rows to the shared snapshot when that is enabled.
    fn publish_snapshot(&self, store: &Store, config: &Config, scope: &str) -> Result<(), RetrievalError>;

    /// Write the rows of every embedded batch, one file at a time from its chunk sidecar. Returns
    /// the number of files written.
    #[cfg(feature = "code-search")]
    fn write_code_rows(&self, store: &mut Store, scope: &str, batches: Vec<PendingCodeBatch>, model: &str) -> usize;

    /// Delete the code rows of `paths`. `false` only when rows that exist could not be deleted.
    #[cfg(feature = "code-search")]
    fn delete_code_rows(&self, store: &mut Store, config: &Config, scope: &str, paths: &[String]) -> bool;
}

/// Per-file deferred-LanceDB-write descriptor. Constructed inside `process_file`'s parallel
/// worker; consumed in the single-threaded apply pass via [`DocumentSink::flush`].
///
/// Deliberately **metadata only** — it carries the content hash needed to re-read the persisted
/// `.doc.msgpack` blob at flush time, NOT the chunk text or the embedding vectors. Accumulating the
/// vectors here (one `Vec<f32>` per chunk, hundreds of chunks per document) across an entire corpus
/// was the memory leak this shape fixes: the flush now rebuilds one file's rows at a time.
#[cfg(feature = "documents")]
#[derive(Debug, Clone)]
pub(crate) struct PendingDocBatch {
    /// Repository-relative path, forward-slash separated. Becomes the `path`
    /// column in LanceDB.
    pub rel_path: String,
    /// Content hash (hex) of the source document — the key under which the `.doc.msgpack` blob is
    /// content-addressed. The flush re-reads that blob to rebuild rows on demand.
    pub blob_hash: String,
    /// LanceDB scope for this file's rows (the repo scope, or `path:<extra_root>` for an
    /// external-root document). Both the delete predicate and the inserted rows use it, so
    /// re-flushing an external-root document replaces its rows instead of leaving stale duplicates.
    pub doc_scope: String,
    /// Number of chunks indexed (zero is valid — xberg may yield no chunks
    /// when the file body is empty or below the chunk threshold).
    pub chunk_count: usize,
    /// Length of each chunk's embedding vector. Zero when embeddings are off.
    pub embedding_dim: u16,
    /// Whether this file should emit LanceDB rows at flush time. False for the no-embed, empty, and
    /// over-`max_chunks_per_document` cases — the blob is still tracked, but nothing lands in LanceDB
    /// (mirrors the old rows-empty batch). Decided at construction, where the document is in hand.
    pub emit_rows: bool,
    /// Whether the doc leaves this pass with its embedding requirement satisfied — persisted onto
    /// the [`crate::store::DocEntry`] so the next pass's unchanged fast path can tell a fully
    /// embedded doc from a tracked-but-vectorless one (issue #44). True when the pass didn't ask to
    /// embed, when vectors are present, or when the doc has no chunks to embed.
    pub embedded: bool,
    /// Whether this pass actually ran an embedding attempt on the doc (embedding requested, freshly
    /// extracted) — even if xberg returned no vectors. Persisted onto
    /// [`crate::store::DocEntry::embed_attempted`] so the unchanged fast path stops re-extracting and
    /// re-embedding a deterministically-unembeddable doc on every scan (issue #44 follow-up). See that
    /// field for the full healing contract.
    pub embed_attempted: bool,
    /// True when this batch came from the cached-blob reuse branch of `extract_and_persist_doc`
    /// rather than a fresh xberg extraction. Drives the `reused_doc_extraction` scan counter — the
    /// observable proof that churn (renames, rewrites) does not re-run extraction or embedding.
    pub reused: bool,
    /// True when the document was chunked by the linear fallback instead of xberg's markdown splitter
    /// (large text-like file; structured data is linear by design and does not count). Drives the `docs_degraded` scan counter.
    pub degraded: bool,
    /// True when structured extraction failed and the file was re-read as `text/plain` (implies
    /// `degraded`). Drives the `docs_text_fallback` scan counter.
    pub text_fallback: bool,
}

/// Where the scan hands a document: extraction, its blob, its chunk rows and their removal.
#[cfg(feature = "documents")]
pub trait DocumentSink: Sync {
    /// Warn once per filter build about `[documents]` settings this build cannot honour.
    fn warn_unavailable_settings(&self, cfg: &DocumentsConfig);

    /// Process one file that has no code grammar: extract it as a document when `[documents]`
    /// admits it. Always returns a result; a file that is not a document is `SkippedNoLang`.
    // Called once per file on the rayon workers: these are the per-file inputs, and an argument
    // struct would only be built and torn down around each call.
    #[allow(clippy::too_many_arguments)]
    fn process(
        &self,
        root: &Path,
        rel: &str,
        filters: &Filters,
        store: &Store,
        config: &Config,
        scope: &str,
        embed: EmbedMode,
    ) -> FileResult;

    /// Timed-out extraction threads still running.
    fn abandoned_extractions(&self) -> usize;

    /// Queue up to `budget` batches for documents an earlier scan left owing rows.
    fn requeue(
        &self,
        store: &mut Store,
        config: &Config,
        filters: &Filters,
        scope: &str,
        batches: &mut Vec<PendingDocBatch>,
        budget: &mut usize,
    );

    /// Write the rows of every batch, one document at a time. Returns the number written.
    fn flush(&self, store: &mut Store, batches: Vec<PendingDocBatch>, model: &str) -> usize;

    /// Drop the entries and rows of documents removed since the last scan.
    fn remove(&self, store: &mut Store, config: &Config, scope: &str, stale: &[String]);

    /// Delete the vectors of documents that are no longer embed-eligible. `false` when rows that
    /// exist could not be deleted.
    fn purge_unembedded(&self, store: &mut Store, config: &Config, filters: &Filters, scope: &str) -> bool;
}

/// The retrieval implementations a scan runs with, bound by `crate::retrieval_wiring`.
#[derive(Clone, Copy)]
pub struct Retrieval {
    #[cfg(feature = "code-search")]
    pub embedder: &'static dyn Embedder,
    pub vectors: &'static dyn VectorStore,
    #[cfg(feature = "documents")]
    pub documents: &'static dyn DocumentSink,
}

impl std::fmt::Debug for Retrieval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Retrieval").finish_non_exhaustive()
    }
}
