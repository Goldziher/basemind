//! Ranked retrieval over the code-search substrate.
//!
//! [`bm25`] is the native Okapi BM25 keyword lane over code chunks — the counterpart to the
//! LanceDB vector lane, sharing the same `chunk_id` document identity. Phase 3 will add an RRF
//! fusion layer (`rrf`) that blends this lane, the vector lane, and an exact symbol lane.

#[cfg(feature = "code-search")]
pub mod bm25;
