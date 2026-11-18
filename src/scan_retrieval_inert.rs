//! A [`Retrieval`] with no retrieval stack behind it, for the core layer's own tests: they may not
//! reach the retrieval modules (`tests/layering.rs`), so a core test scans with no embedder, no
//! vector store and no document lane. A test of what those do lives with them, in the retrieval
//! layer.

#[cfg(any(feature = "documents", feature = "code-search"))]
use std::path::Path;

use super::Retrieval;
#[cfg(any(feature = "documents", feature = "code-search"))]
use super::{RetrievalError, VectorStore};
#[cfg(any(feature = "documents", feature = "code-search"))]
use crate::config::Config;
#[cfg(any(feature = "documents", feature = "code-search"))]
use crate::store::Store;

/// The seams of a build without a retrieval stack.
#[cfg(any(feature = "documents", feature = "code-search"))]
struct Inert;

impl Retrieval {
    /// No embedder, vector store or document lane: a code embedding fails, no vector store opens,
    /// and every file without a grammar is skipped.
    pub(crate) fn inert() -> Self {
        Self {
            #[cfg(feature = "code-search")]
            embedder: &Inert,
            #[cfg(feature = "code-search")]
            model_key: |preset| preset.to_string(),
            #[cfg(any(feature = "documents", feature = "code-search"))]
            vectors: &Inert,
            #[cfg(feature = "documents")]
            documents: &Inert,
        }
    }
}

#[cfg(any(feature = "documents", feature = "code-search"))]
fn absent() -> RetrievalError {
    RetrievalError("no retrieval stack in this scan".to_string())
}

#[cfg(feature = "code-search")]
impl super::Embedder for Inert {
    fn dim(&self, _spec: &super::EmbedderSpec<'_>) -> Result<u16, RetrievalError> {
        Err(absent())
    }

    fn embed_batch(&self, _spec: &super::EmbedderSpec<'_>, _texts: &[&str]) -> Result<Vec<Vec<f32>>, RetrievalError> {
        Err(absent())
    }
}

#[cfg(any(feature = "documents", feature = "code-search"))]
impl VectorStore for Inert {
    fn open(&self, _store: &mut Store, _config: &Config) -> Result<(), RetrievalError> {
        Err(absent())
    }

    fn store_id_at(&self, _dir: &Path) -> Option<String> {
        None
    }

    fn open_store_id(&self, _store: &Store) -> Option<String> {
        None
    }

    fn close(&self, _store: &mut Store) {}

    fn maintain(&self, _store: &Store) {}

    #[cfg(feature = "code-search")]
    fn write_code_rows(
        &self,
        _store: &mut Store,
        _scope: &str,
        _batches: Vec<crate::scanner_code::PendingCodeBatch>,
        _model: &str,
    ) -> usize {
        0
    }

    #[cfg(feature = "code-search")]
    fn delete_code_rows(&self, _store: &mut Store, _config: &Config, _scope: &str, _paths: &[String]) -> bool {
        true
    }
}

#[cfg(feature = "documents")]
impl super::DocumentSink for Inert {
    fn warn_unavailable_settings(&self, _cfg: &crate::config::DocumentsConfig) {}

    fn process(
        &self,
        _root: &Path,
        rel: &str,
        _filters: &crate::scanner_filter::Filters,
        _store: &Store,
        _config: &Config,
        _scope: &str,
        _embed: crate::scanner::EmbedMode,
    ) -> crate::scanner::FileResult {
        crate::scanner::FileResult::bare(rel.to_string(), crate::scanner::FileStatus::SkippedNoLang)
    }

    fn abandoned_extractions(&self) -> usize {
        0
    }

    fn embedder_ready(&self, _config: &Config) -> bool {
        false
    }

    fn requeue(
        &self,
        _store: &mut Store,
        _config: &Config,
        _filters: &crate::scanner_filter::Filters,
        _scope: &str,
        _batches: &mut Vec<super::PendingDocBatch>,
        _budget: &mut usize,
    ) {
    }

    fn flush(
        &self,
        _store: &mut Store,
        _config: &Config,
        _filters: &crate::scanner_filter::Filters,
        _batches: Vec<super::PendingDocBatch>,
        _budget: usize,
    ) -> usize {
        0
    }

    fn remove(
        &self,
        _store: &mut Store,
        _config: &Config,
        _filters: &crate::scanner_filter::Filters,
        _stale: &[String],
    ) {
    }

    fn purge_unembedded(
        &self,
        _store: &mut Store,
        _config: &Config,
        _filters: &crate::scanner_filter::Filters,
        _scope: &str,
    ) -> bool {
        true
    }
}
