//! Internal request shapes and response payloads of the code-search and read bodies.
//!
//! The wire arguments live in `types_tools.rs`; [`SearchCodeParams`], [`GetChunkParams`] and
//! [`ExpandParams`] are what the bodies in `helpers_code_search.rs` / `helpers_expand.rs` take.
//!
//! The response structs are gated on `code-search` since only the feature-on helper bodies build
//! them.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::path::RelPath;

/// Params for the `semantic` mode — hybrid / vector / BM25 retrieval over indexed code chunks.
#[cfg(feature = "code-search")]
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchCodeParams {
    pub query: String,
    /// Max hits to return. Default 10, max 100.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional token budget bounding the returned `hits` list (best-first; sets `budgeted`).
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Wire format: `"json"` (default) or `"toon"`. Overrides the `[documents.output] format`
    /// config knob for this call.
    #[serde(default)]
    pub format: Option<String>,
    /// Retrieval lane: "hybrid" (RRF fusion of the vector + keyword + exact-symbol lanes — the
    /// default), "semantic" (vector KNN only), or "keyword" (native BM25 only). Hybrid degrades
    /// gracefully — it drops any lane that is unavailable (e.g. the vector lane without embeddings).
    #[serde(default)]
    pub mode: Option<String>,
    /// Per-query override: run the cross-encoder rerank pass over the fused hits. Defaults to the
    /// `[code_search.reranker] enabled` config knob. The first rerank downloads an ONNX model.
    #[serde(default)]
    pub reranker_enabled: Option<bool>,
    /// Per-query override: the xberg reranker preset (e.g. `bge-reranker-base`).
    #[serde(default)]
    pub reranker_preset: Option<String>,
    /// Per-query override: how many top fused hits to rerank.
    #[serde(default)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// 1-based first line of the chunk.
    #[serde(rename = "line", alias = "line_start")]
    pub line_start: u32,
    /// 1-based last line of the chunk, inclusive.
    #[serde(rename = "end_line", alias = "line_end")]
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
    /// Why-matched provenance (hybrid mode only): which lanes produced this hit, in fixed lane
    /// order `exact` → `vector` → `keyword` (only lanes that ranked the chunk appear). Lets an agent
    /// see whether a hit is an exact-symbol match, a semantic neighbor, a lexical match, or an
    /// agreement across lanes. Not sorted by contribution — read the per-lane ranks below for that.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_lanes: Vec<String>,
    /// 1-based rank this chunk held in the keyword (BM25) lane, when that lane ranked it (hybrid).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyword_rank: Option<u32>,
    /// 1-based rank this chunk held in the vector (semantic) lane, when that lane ranked it (hybrid).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_rank: Option<u32>,
    /// 1-based rank this chunk held in the exact (symbol) lane, when that lane ranked it (hybrid).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_rank: Option<u32>,
}

/// A retrieval lane that did not run for a query, and why.
#[cfg(feature = "code-search")]
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub(crate) struct LaneUnavailable {
    pub lane: &'static str,
    pub reason: String,
}

#[cfg(feature = "code-search")]
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct SearchCodeResponse {
    pub query: String,
    /// True when a `max_tokens` budget dropped trailing `hits`. No cursor — raise `max_tokens`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub hits: Vec<CodeSearchHit>,
    /// Lanes that produced a ranking for this query: `vector`, `keyword`, `exact`.
    pub lanes_ran: Vec<&'static str>,
    /// Lanes that did NOT run, each with why. Non-empty means `hits` is a PARTIAL result from the
    /// lanes that did run: a caller must be able to tell "searched and found nothing" from "could
    /// not search". When NO lane runs the call is an error instead, so this is never a labelled
    /// `[]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lanes_unavailable: Vec<LaneUnavailable>,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / vector
    /// search + response construction), excluding MCP transport, argument
    /// deserialization, and response serialization. A first call against a cold server also
    /// includes index warm-up; such responses carry a `notice`. See
    /// [`crate::mcp::helpers::timing`] for the full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

/// Response for `get_chunk` — the full chunk body plus its metadata.
#[cfg(feature = "code-search")]
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Server-side handler latency in microseconds — the tool body's own execution (index / vector
    /// search + response construction), excluding MCP transport, argument
    /// deserialization, and response serialization. A first call against a cold server also
    /// includes index warm-up; such responses carry a `notice`. See
    /// [`crate::mcp::helpers::timing`] for the full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}

/// Parameters for the `expand` MCP tool.
///
/// Resolves one symbol by `name` (and optionally `kind`) in the file's L1 outline, then
/// reads the source file from disk and returns the raw bytes in `[start_byte..end_byte]`.
/// When more than one symbol matches `name` (e.g. overloads), the tool returns an error
/// listing the matches so the caller can disambiguate by supplying `kind`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ExpandParams {
    /// Repo-relative path of the indexed source file.
    pub path: RelPath,
    /// Symbol name to expand. Matched exactly (case-sensitive) against the L1 outline.
    pub name: String,
    /// Optional kind filter to disambiguate when `name` matches multiple symbols
    /// (e.g. `"function"`, `"method"`, `"struct"`, …). Same values as `search_symbols`.
    #[serde(default)]
    pub kind: Option<String>,
    /// 1-based line inside the definition meant, when several in the file share the name.
    #[serde(default)]
    pub line: Option<u32>,
}

/// Response from the `expand` MCP tool.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct ExpandResponse {
    /// Repo-relative path of the file that was read.
    pub path: String,
    /// Resolved symbol name.
    pub name: String,
    /// Resolved symbol kind (e.g. `"function"`, `"struct"`).
    pub kind: String,
    /// 1-based first line of the symbol.
    pub line: u32,
    /// 1-based last line of the returned body, inclusive: the symbol's last line unless `truncated`.
    pub end_line: u32,
    /// Raw source text of the symbol body (`file_bytes[start_byte..end_byte]`).
    pub body: String,
    /// Byte length of the returned body (before any truncation).
    pub bytes: usize,
    /// `true` when the body was truncated to the `EXPAND_BODY_CAP`.
    pub truncated: bool,
    /// The file changed since it was indexed, so the symbol was located by parsing the working-tree
    /// file; the other tools describe the indexed content until `index_rescan`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
}

/// Response of a `code_read` line-span read.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct ReadSpanResponse {
    pub path: String,
    /// 1-based first line returned.
    pub line: u32,
    /// 1-based last line returned, inclusive: clamped to the file, and to what fits when `truncated`.
    pub end_line: u32,
    pub total_lines: u32,
    pub body: String,
    /// Byte length of the span before any truncation.
    pub bytes: usize,
    /// `true` when the body was cut at the size cap.
    pub truncated: bool,
}
