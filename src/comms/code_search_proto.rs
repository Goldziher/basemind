//! Wire types for forwarding the code-search keyword + exact lanes from a reader `serve` to the
//! machine daemon.
//!
//! Both lanes are answered from fjall — BM25 postings for the keyword lane, `symbols_by_name` for
//! the exact lane — and fjall's directory lock is exclusive, so a reader session opens the store
//! without an `IndexDb` and cannot run either. Before this forward that was a SILENT degradation:
//! `code` mode `semantic` returned an empty hit list in microseconds, indistinguishable from "no
//! matches", even though `[code_search] enabled` defaults to true and the config documents the
//! keyword lane as working without embeddings.
//!
//! Only the RANKING crosses the wire. Chunk bodies live in content-addressed blobs, which a reader
//! can read concurrently, so the caller hydrates hits itself and keeps RRF fusion and reranking
//! local — the daemon never serializes chunk text.

#![cfg(all(feature = "comms", any(unix, windows)))]

use serde::{Deserialize, Serialize};

/// A ranked lane lookup forwarded to the daemon, answered against the workspace's read-write fjall
/// index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeSearchLaneQuery {
    /// The raw user query; tokenization is the daemon's job so both sides cannot drift.
    pub query: String,
    /// Per-lane cap. The caller passes its fusion limit, not its final `limit`.
    pub limit: u32,
    /// Whether the exact symbol-name lane is wanted alongside BM25. `false` skips a
    /// `symbols_by_name` scan the caller would only discard.
    pub want_exact: bool,
}

/// The daemon's answer to a [`CodeSearchLaneQuery`]: ranked chunk ids, best first.
///
/// Carries no bodies by design (see the module docs). Not `Eq` — the BM25 score is an `f32`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodeSearchLaneResult {
    /// BM25-ranked `(chunk_id, score)`.
    pub keyword: Vec<(String, f32)>,
    /// Exact symbol-name lane chunk ids. Empty when `want_exact` was false OR when the query is not
    /// identifier-shaped — the caller cannot tell the two apart, and does not need to.
    pub exact: Vec<String>,
}
