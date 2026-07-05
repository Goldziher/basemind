//! Parameter + response shapes for the semantic code-search tools (`search_code`, `get_chunk`).
//!
//! The `*Params` structs are always compiled (the tool shims + CLI reference them regardless of
//! the `code-search` feature). The response structs are gated on `code-search` since only the
//! feature-on helper bodies build them.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::path::RelPath;

/// Params for `search_code` — vector KNN over indexed code chunks.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchCodeParams {
    #[serde(alias = "needle", alias = "pattern", alias = "q", alias = "text", alias = "search")]
    pub query: String,
    /// Max hits to return. Default 10, max 100.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned `hits` list (best-first; sets `budgeted`).
    #[serde(default, alias = "token_budget", alias = "budget")]
    pub max_tokens: Option<u32>,
    /// Wire format: `"json"` (default) or `"toon"`. Overrides the `[documents.output] format`
    /// config knob for this call.
    #[serde(default, alias = "encoding")]
    pub format: Option<String>,
    /// Retrieval lane: "hybrid" (RRF fusion of the vector + keyword + exact-symbol lanes — the
    /// default), "semantic" (vector KNN only), or "keyword" (native BM25 only). Hybrid degrades
    /// gracefully — it drops any lane that is unavailable (e.g. the vector lane without embeddings).
    #[serde(default, alias = "lane")]
    pub mode: Option<String>,
    /// Per-query override: run the cross-encoder rerank pass over the fused hits. Defaults to the
    /// `[code_search.reranker] enabled` config knob. The first rerank downloads an ONNX model.
    #[serde(default, alias = "rerank")]
    pub reranker_enabled: Option<bool>,
    /// Per-query override: the xberg reranker preset (e.g. `bge-reranker-base`).
    #[serde(default, alias = "rerank_preset")]
    pub reranker_preset: Option<String>,
    /// Per-query override: how many top fused hits to rerank.
    #[serde(default, alias = "rerank_top_k")]
    pub reranker_top_k: Option<usize>,
}

/// Params for `get_chunk` — fetch a chunk body by path (the `search_code` pointer).
///
/// `path` is required (every `search_code` hit carries it). Disambiguate within the file with
/// `chunk_id` or `byte_start`; when the file has exactly one chunk both may be omitted.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GetChunkParams {
    /// Repository-relative path of the source file the chunk belongs to.
    pub path: RelPath,
    /// The content-addressed chunk id from a `search_code` hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_id: Option<String>,
    /// Alternatively, the chunk's start byte offset from a `search_code` hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_start: Option<u32>,
}

/// One pointer hit from `search_code`. Deliberately carries NO body — call `get_chunk` for the
/// source. Mirrors the `search_symbols`/`outline` → `expand` two-call token pattern.
#[cfg(feature = "code-search")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CodeSearchHit {
    pub path: String,
    pub chunk_id: String,
    /// Symbol name; empty for a module-level chunk.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub symbol: String,
    /// Symbol kind (`function`, `method`, `module`, …).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    pub lang: String,
    pub line_start: u32,
    pub line_end: u32,
    pub byte_start: u32,
    pub byte_end: u32,
    /// L2 distance from the query vector (lower = closer). Semantic lane only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distance: Option<f32>,
    /// BM25 relevance score (higher = better). Keyword lane only. In hybrid mode this carries the
    /// fused RRF score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    /// Cross-encoder rerank score (higher = better). Present only when the rerank pass ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank_score: Option<f32>,
}

#[cfg(feature = "code-search")]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SearchCodeResponse {
    pub query: String,
    /// True when a `max_tokens` budget dropped trailing `hits`. No cursor — raise `max_tokens`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub hits: Vec<CodeSearchHit>,
}

/// Response for `get_chunk` — the full chunk body plus its metadata.
#[cfg(feature = "code-search")]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct GetChunkResponse {
    pub path: String,
    pub chunk_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub lang: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    pub line_start: u32,
    pub line_end: u32,
    pub byte_start: u32,
    pub byte_end: u32,
    pub text: String,
}
