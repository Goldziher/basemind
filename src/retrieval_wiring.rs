//! Binds the scan's retrieval seams (`crate::scan_retrieval`) to the implementations this build
//! compiles in. It belongs to the engine layer (ADR-0023): the engine is what chooses whether an
//! embedding application gets the document and vector stack. Every scan entry point takes the
//! binding as an argument, and the engine and the binaries pass [`backend`], so no core module
//! reaches the retrieval layer to pick an implementation.

use crate::scan_retrieval::Retrieval;

/// The identity of `preset`'s model that stored vectors carry ([`crate::embeddings::model_key`]).
#[cfg(feature = "code-search")]
fn model_key(preset: &str) -> String {
    crate::embeddings::model_key(preset).into_owned()
}

/// The retrieval implementations of this build: the embedder, vector store and document lane its
/// features compile in, or none.
pub fn backend() -> Retrieval {
    Retrieval {
        #[cfg(feature = "code-search")]
        embedder: &crate::embeddings::ScanEmbedder,
        #[cfg(feature = "code-search")]
        model_key,
        #[cfg(any(feature = "documents", feature = "code-search"))]
        vectors: &crate::scanner_vectors::LanceVectors,
        #[cfg(feature = "documents")]
        documents: &crate::scanner_docs_lane::DocumentLane,
    }
}
