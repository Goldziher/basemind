//! The workspace's retrieval handles: the embedding model and the code-chunk vector store, each
//! loaded on the first call that needs it and kept for the workspace's life.
//!
//! Loading a model reads it from disk (or downloads it), and an open can wait out another holder of
//! the store. Both run on the calling thread, which is never a runtime worker: the adapter runs
//! every operation on a blocking thread, where the vector store's own `block_on` is allowed.

#![cfg(any(feature = "memory", feature = "documents", feature = "code-search"))]

use std::sync::{Arc, PoisonError};

use super::Workspace;
#[cfg(any(feature = "memory", feature = "code-search"))]
use super::error::EngineError;
use crate::embeddings::SharedEmbedder;

impl Workspace {
    /// The shared embedding engine, loaded on first use. Callers that arrive during the load wait
    /// for it; a load that fails is tried again by the next call.
    pub(crate) fn shared_embedder(&self) -> Result<Arc<SharedEmbedder>, String> {
        if let Some(embedder) = self.embedder.get() {
            return Ok(Arc::clone(embedder));
        }
        let _loading = self.embedder_loading.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(embedder) = self.embedder.get() {
            return Ok(Arc::clone(embedder));
        }
        let config = &self.config;
        let loaded = SharedEmbedder::load_with_provider(
            &config.documents.embedding_preset,
            config
                .resources
                .effective_embed_threads(config.documents.embed_max_threads),
            config.resources.embed_batch_size,
            config.resources.onnx_provider,
        )
        .map(Arc::new)
        .map_err(|e| format!("load embedder: {e}"))?;
        Ok(Arc::clone(self.embedder.get_or_init(|| loaded)))
    }

    /// Embed a search query, with the preset's query prefix (see `SharedEmbedder::embed_query`).
    #[cfg(any(feature = "memory", feature = "code-search"))]
    pub(crate) fn embed_query(&self, text: &str) -> Result<Vec<f32>, EngineError> {
        self.embed_with(text, true)
    }

    /// Embed a passage to be found later (a memory note): no query prefix.
    #[cfg(feature = "memory")]
    pub(crate) fn embed_passage(&self, text: &str) -> Result<Vec<f32>, EngineError> {
        self.embed_with(text, false)
    }

    /// An empty text is the caller's to fix; a model that will not load is unavailable until it
    /// can be loaded (the embedder remembers the failure for a while). Anything else is internal.
    #[cfg(any(feature = "memory", feature = "code-search"))]
    fn embed_with(&self, text: &str, query: bool) -> Result<Vec<f32>, EngineError> {
        require_text(text, query)?;
        let embedder = self.shared_embedder().map_err(EngineError::Unavailable)?;
        let embedded = if query {
            embedder.embed_query(text)
        } else {
            embedder.embed(text)
        };
        embedded.map_err(embed_error)
    }

    /// The workspace's code-chunk vector store, opened on first use. Only a writer (holding the
    /// workspace lock) creates or rebuilds it; any other open reads a matching one
    /// ([`crate::lance::LanceStore::open_existing`]).
    #[cfg(feature = "code-search")]
    pub(crate) fn lance_store(&self) -> Result<Arc<crate::lance::LanceStore>, super::lance_cell::Unavailable> {
        self.lance.get_or_open(|| {
            let embedder = self.shared_embedder().map_err(|e| format!("embedder init: {e}"))?;
            let (dir, writer) = {
                let store = self.store_read();
                (
                    store.basemind_dir.join(crate::store::LANCE_DIR),
                    store.holds_writer_lock(),
                )
            };
            let (dim, model) = (embedder.dim(), embedder.model());
            let opened = if writer {
                crate::lance::LanceStore::open(&dir, dim, model)
            } else {
                crate::lance::LanceStore::open_existing(&dir, dim, model)
            };
            opened.map(Arc::new).map_err(|e| format!("open LanceStore: {e}"))
        })
    }
}

/// Refuse an empty text before any model is loaded: there is nothing to embed.
#[cfg(any(feature = "memory", feature = "code-search"))]
fn require_text(text: &str, query: bool) -> Result<(), EngineError> {
    if !text.trim().is_empty() {
        return Ok(());
    }
    Err(EngineError::invalid_argument(if query {
        "`query` is empty: pass the text to search for"
    } else {
        "the text to embed is empty"
    }))
}

/// The class of an embedding failure: a model that will not load is unavailable, anything else is
/// internal.
#[cfg(any(feature = "memory", feature = "code-search"))]
fn embed_error(error: anyhow::Error) -> EngineError {
    let message = format!("embed: {error:#}");
    if crate::embeddings::is_load_failure(&error) {
        EngineError::unavailable(message)
    } else {
        EngineError::internal(message)
    }
}

#[cfg(all(test, any(feature = "memory", feature = "code-search")))]
mod tests {
    use super::*;

    /// An empty `memory_search` query and an embedder that will not load were reported as internal
    /// errors (review 3): the first is the caller's to fix, the second is unavailable for now.
    #[test]
    fn embedding_failures_are_classed_by_what_the_caller_can_do() {
        assert!(matches!(require_text("  ", true), Err(EngineError::InvalidArgument(_))));
        assert!(matches!(require_text("", false), Err(EngineError::InvalidArgument(_))));
        assert!(require_text("rivers", true).is_ok());
        let offline = anyhow::anyhow!("failed to download model from huggingface");
        assert!(matches!(embed_error(offline), EngineError::Unavailable(_)));
        let other = anyhow::anyhow!("embed_texts returned empty result");
        assert!(matches!(embed_error(other), EngineError::Internal(_)));
    }
}
