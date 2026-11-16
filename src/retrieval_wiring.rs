//! Binds the scan's retrieval seams ([`crate::scan_retrieval`]) to the implementations this build
//! compiles in. It belongs to the engine layer (ADR-0023): the engine is what chooses whether an
//! embedding application gets the document and vector stack. Until the engine API exists the scan
//! asks for the binding here; once it does, the engine passes it in and this call goes away.

#![cfg(any(feature = "documents", feature = "code-search"))]

use crate::scan_retrieval::Retrieval;

/// The retrieval implementations of this build.
pub(crate) fn backend() -> Retrieval {
    Retrieval {
        #[cfg(feature = "code-search")]
        embedder: &crate::embeddings::ScanEmbedder,
        vectors: &crate::scanner_vectors::LanceVectors,
        #[cfg(feature = "documents")]
        documents: &crate::scanner_docs_lane::DocumentLane,
    }
}
