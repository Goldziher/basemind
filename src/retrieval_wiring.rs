//! Binds the scan's retrieval seams ([`crate::scan_retrieval`]) to the implementations this build
//! compiles in. It belongs to the engine layer (ADR-0023): the engine is what chooses whether an
//! embedding application gets the document and vector stack. The core scan names only its own
//! [`Retrieval`] type and takes [`Retrieval::default`], which this module provides, so no core module
//! reaches the retrieval layer to pick an implementation.

#![cfg(any(feature = "documents", feature = "code-search"))]

use crate::scan_retrieval::Retrieval;

/// The identity of `preset`'s model that stored vectors carry ([`crate::embeddings::model_key`]).
#[cfg(feature = "code-search")]
fn model_key(preset: &str) -> String {
    crate::embeddings::model_key(preset).into_owned()
}

/// The retrieval implementations of this build.
impl Default for Retrieval {
    fn default() -> Self {
        Self {
            #[cfg(feature = "code-search")]
            embedder: &crate::embeddings::ScanEmbedder,
            #[cfg(feature = "code-search")]
            model_key,
            vectors: &crate::scanner_vectors::LanceVectors,
            #[cfg(feature = "documents")]
            documents: &crate::scanner_docs_lane::DocumentLane,
        }
    }
}
