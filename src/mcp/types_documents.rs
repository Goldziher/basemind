//! Documents-tier parameter + response shapes for `search_documents` and friends.
//!
//! Extracted from `src/mcp/types.rs` so the parent module stays under the per-file line
//! cap as more documents-tier types are added in later iters. Re-exported wholesale via
//! `pub use types_documents::*;` in `types.rs`.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchDocumentsParams {
    #[serde(alias = "needle", alias = "pattern", alias = "q", alias = "text", alias = "search")]
    pub query: String,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned `hits` list (not the whole envelope).
    /// Hits are kept best-first until the budget is hit; the rest are dropped and the
    /// response carries `budgeted: true`.
    #[serde(default, alias = "token_budget", alias = "budget")]
    pub max_tokens: Option<u32>,
    /// Wire format for the response: `"json"` (default) or `"toon"`. TOON is a compact
    /// tabular encoding of the `hits` list — far fewer tokens than JSON for large hit sets.
    /// Overrides the `[documents.output] format` config knob for this call.
    #[serde(default, alias = "encoding")]
    pub format: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    /// Which ingestion scope to search. Defaults to this repo's scope — the one the
    /// scanner's document lane writes under. Web pages are ingested under a **different**
    /// scope (`web:<host>`, echoed back by `web_scrape` / `web_crawl`), so reaching them
    /// requires naming it here; without this, a scraped page is written, stored, and then
    /// permanently invisible to search.
    #[serde(default)]
    pub scope: Option<String>,
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
pub(crate) use crate::extract::doc::{DocEntity, DocKeyword, DocSummary};

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
    /// Document-level summary from the parent doc blob, when summarisation was
    /// enabled at scan time. `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<DocSummary>,
}

#[cfg(feature = "documents")]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SearchDocumentsResponse {
    pub query: String,
    /// True when a `max_tokens` budget dropped trailing `hits`. `search_documents` has no
    /// cursor; raise `max_tokens` (or omit it) to retrieve more hits.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub hits: Vec<DocumentSearchHit>,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / vector
    /// search / graph walk + response construction), excluding MCP transport, argument
    /// deserialization, and response serialization. A first call against a cold server also
    /// includes index warm-up; such responses carry a `notice`. See
    /// [`crate::mcp::helpers::timing`] for the full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}
