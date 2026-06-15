//! Documents-tier parameter + response shapes for `search_documents` and friends.
//!
//! Extracted from `src/mcp/types.rs` so the parent module stays under the per-file line
//! cap as more documents-tier types are added in later iters. Re-exported wholesale via
//! `pub use types_documents::*;` in `types.rs`.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchDocumentsParams {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub mime_type: Option<String>,
    /// Per-query overrides for any `documents.*` config knob. Takes precedence over
    /// serve-time config and CLI flags. Known override fields (mirroring `[documents]`)
    /// are applied; unrecognized fields are silently ignored — flatten semantics
    /// (`#[serde(flatten)]` and `deny_unknown_fields` are mutually exclusive in serde).
    #[serde(flatten, default)]
    pub overrides: crate::config::DocumentsCliOverrides,
}

#[cfg(feature = "documents")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DocumentSearchHit {
    pub path: String,
    pub chunk_idx: u32,
    pub text: String,
    pub mime_type: String,
    pub byte_start: u32,
    pub byte_end: u32,
    pub distance: f32,
    /// Cross-encoder relevance score in `[0, 1]`. Present only when the reranker is
    /// enabled on the call; absent (`null` / omitted) when reranker is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank_score: Option<f32>,
}

#[cfg(feature = "documents")]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SearchDocumentsResponse {
    pub query: String,
    pub hits: Vec<DocumentSearchHit>,
}
