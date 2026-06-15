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
    /// Optional case-insensitive substring match against `DocEntity.category`.
    /// When set, only hits whose parent document carries at least one entity
    /// in that category are returned. Combined with `keywords_contains` via
    /// AND semantics (both must match when both are set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_category: Option<String>,
    /// Optional case-insensitive substring match against `DocKeyword.text`.
    /// When set, only hits whose parent document carries at least one keyword
    /// containing the substring are returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keywords_contains: Option<String>,
    /// Per-query overrides for any `documents.*` config knob. Takes precedence over
    /// serve-time config and CLI flags. Known override fields (mirroring `[documents]`)
    /// are applied; unrecognized fields are silently ignored — flatten semantics
    /// (`#[serde(flatten)]` and `deny_unknown_fields` are mutually exclusive in serde).
    #[serde(flatten, default)]
    pub overrides: crate::config::DocumentsCliOverrides,
}

#[cfg(feature = "documents")]
pub(crate) use crate::extract::doc::{DocEntity, DocKeyword};

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
    /// Keywords from the parent document, when keyword extraction was enabled
    /// at scan time. Empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<DocKeyword>,
    /// Named entities from the parent document, when NER was enabled at scan time.
    /// Empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<DocEntity>,
}

#[cfg(feature = "documents")]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SearchDocumentsResponse {
    pub query: String,
    pub hits: Vec<DocumentSearchHit>,
}
