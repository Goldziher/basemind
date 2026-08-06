//! Request shape for the consolidated `code` domain tool, plus the semantic-search payloads.
//!
//! [`CodeParams`] is what crosses the wire: one flat parameter object with a required [`CodeMode`]
//! selecting the lookup and every per-mode field an optional sibling. The per-operation structs
//! ([`SearchCodeParams`] / [`GetChunkParams`] here, `OutlineParams` / `SearchSymbolsParams` /
//! `ListFilesParams` / `FindFilesParams` / `FindReferencesParams` / `FindCallersParams` /
//! `GotoDefinitionParams` / `WorkspaceGrepParams` in `types.rs`, `FindImplementationsParams` in
//! `types_impls.rs`, `ExpandParams` in `types_compress.rs`) stay as the helpers' internal shapes, so
//! the bodies keep taking exactly the arguments they always did.
//!
//! `CodeParams` lives here rather than in `types.rs` because that file is already within ~90 lines
//! of the 1000-line per-file cap `tests/max_lines.rs` enforces.
//!
//! The response structs are gated on `code-search` since only the feature-on helper bodies build
//! them.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::cursor::Cursor;
use super::mode::CodeMode;
use crate::path::RelPath;

/// Wire parameters for the `code` tool.
///
/// Only `mode` is required. Every other field belongs to a subset of the modes and is rejected —
/// not ignored — when passed to a mode that has no use for it (see
/// [`super::mode::reject_unsupported`]); a mode that needs one names the exact `mode`/field pair.
/// Per-mode defaults are resolved in the helper, not here, because they differ by mode (`limit`
/// defaults to 100 for the symbol/reference scanners, 200 for the file listers, 10 for `semantic`).
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CodeParams {
    /// Which lookup to run.
    pub mode: CodeMode,
    /// Repo-relative path of the file to act on. Required by `outline`, `definition`, `callers`,
    /// `expand` and `chunk`.
    #[serde(default)]
    pub path: Option<RelPath>,
    /// Symbol name. `symbols` matches it as a case-sensitive SUBSTRING of every indexed symbol
    /// name; `references` matches it against captured callee identifiers (also a substring, and
    /// name-only — `Foo::bar()` and `bar()` both match `"bar"`); `callers` and `expand` match it
    /// exactly against the definitions in `path`. Required by those four modes.
    #[serde(default, alias = "needle", alias = "symbol", alias = "q")]
    pub name: Option<String>,
    /// Free-text query. `find` matches it as a fuzzy subsequence against every indexed path;
    /// `semantic` embeds / tokenizes it for retrieval. Required by both.
    #[serde(default, alias = "text")]
    pub query: Option<String>,
    /// `grep` only. Rust regex syntax (`regex` crate). Required by that mode.
    #[serde(default, alias = "regex", alias = "search")]
    pub pattern: Option<String>,
    /// `implementations` only. Trait / interface / base-class name, matched as a case-sensitive
    /// substring. Required by that mode.
    #[serde(default, alias = "trait", alias = "interface")]
    pub trait_name: Option<String>,
    /// `dependents` only. Module / import target (e.g. `"tokio::sync"`, `"react"`), matched as a
    /// substring against each recorded import path. Required by that mode.
    #[serde(default, alias = "import")]
    pub module: Option<String>,
    /// `symbols` / `callers` / `expand`. Symbol-kind filter: function, method, struct, enum, class,
    /// interface, trait, type, const, module, macro.
    #[serde(default)]
    pub kind: Option<String>,
    /// `grep` / `files` / `find` / `implementations`. Language filter (e.g. `"rust"`,
    /// `"typescript"`), applied before matching.
    #[serde(default)]
    pub language: Option<String>,
    /// `grep` / `files`. Substring filter on the repo-relative path.
    #[serde(default)]
    pub path_contains: Option<String>,
    /// `find` only. Path prefix applied before fuzzy scoring (e.g. `"src/mcp/"`).
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// `definition` only. 1-based line of the reference identifier. Required by that mode.
    #[serde(default, alias = "row")]
    pub line: Option<u32>,
    /// `definition` only. 0-based byte column of the reference within the line. Default 0.
    #[serde(default, alias = "col")]
    pub column: Option<u32>,
    /// `outline` only. Also return calls + doc comments (L2). Default false; falls back to empty
    /// lists when no L2 blob exists for the file's current content.
    #[serde(default)]
    pub l2: Option<bool>,
    /// `grep` only. Include one line of context before and after each hit. Default true.
    #[serde(default)]
    pub include_context: Option<bool>,
    /// Result cap, per mode: `symbols` / `grep` / `references` / `callers` / `implementations`
    /// default 100, max 1000; `files` / `find` default 200, max 5000; `semantic` default 10,
    /// max 100.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Token budget bounding the returned list (never the whole envelope). Entries are kept in
    /// result order until the budget is hit; the rest are dropped and the response carries
    /// `budgeted: true` plus, where the mode pages, a `next_cursor`.
    #[serde(default, alias = "token_budget", alias = "budget")]
    pub max_tokens: Option<u32>,
    /// Wire format for the response: `"json"` (default) or `"toon"` — a compact tabular encoding of
    /// the result list, far fewer tokens than JSON for large result sets.
    #[serde(default, alias = "encoding")]
    pub format: Option<String>,
    /// Resume token from the previous call's `next_cursor`. Fjall-backed modes (`references`,
    /// `callers`, `implementations`) keep cursors stable across rescans; in-memory modes
    /// (`symbols`, `grep`, `files`, `find`) invalidate them, setting `cursor_invalidated`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// `semantic` only. Retrieval lane: `"hybrid"` (default — RRF fusion of the vector, keyword and
    /// exact-symbol lanes), `"semantic"` (vector KNN only), or `"keyword"` (native BM25 only).
    /// Named `lane` rather than `mode` because `mode` selects the domain operation here.
    #[serde(default, alias = "strategy")]
    pub lane: Option<String>,
    /// `semantic` only. Run the cross-encoder rerank pass over the fused hits. Defaults to the
    /// `[code_search.reranker] enabled` config knob; the first rerank downloads an ONNX model.
    #[serde(default, alias = "reranker_enabled")]
    pub rerank: Option<bool>,
    /// `semantic` only. Reranker preset name (default `bge-reranker-base`).
    #[serde(default, alias = "reranker_preset")]
    pub rerank_preset: Option<String>,
    /// `semantic` only. How many top fused hits to rerank.
    #[serde(default, alias = "reranker_top_k")]
    pub rerank_top_k: Option<usize>,
    /// `chunk` only. The content-addressed chunk id from a `semantic` hit.
    #[serde(default)]
    pub chunk_id: Option<String>,
    /// `chunk` only. The chunk's start byte offset from a `semantic` hit — an alternative to
    /// `chunk_id`. Both may be omitted when the file holds a single chunk.
    #[serde(default)]
    pub byte_start: Option<u32>,
}

impl CodeParams {
    /// A call carrying only `mode`. Callers set the fields their mode uses and leave the rest
    /// `None`: the helper rejects a field belonging to another mode, so populating them blindly
    /// would fail the call.
    pub fn new(mode: CodeMode) -> Self {
        Self {
            mode,
            path: None,
            name: None,
            query: None,
            pattern: None,
            trait_name: None,
            module: None,
            kind: None,
            language: None,
            path_contains: None,
            path_prefix: None,
            line: None,
            column: None,
            l2: None,
            include_context: None,
            limit: None,
            max_tokens: None,
            format: None,
            cursor: None,
            lane: None,
            rerank: None,
            rerank_preset: None,
            rerank_top_k: None,
            chunk_id: None,
            byte_start: None,
        }
    }
}

/// Params for the `semantic` mode — hybrid / vector / BM25 retrieval over indexed code chunks.
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

#[cfg(feature = "code-search")]
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct SearchCodeResponse {
    pub query: String,
    /// True when a `max_tokens` budget dropped trailing `hits`. No cursor — raise `max_tokens`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    pub hits: Vec<CodeSearchHit>,
    /// Lanes that did NOT run for this query — `"keyword"` and/or `"exact"`. Non-empty means `hits`
    /// is a PARTIAL result from the lanes that did run, not the whole picture; an empty list means
    /// every requested lane ran. Machine-readable on purpose: a caller must be able to tell
    /// "searched and found nothing" from "could not search", which a prose-only notice does not
    /// allow. When NO lane runs the call is an error instead, so this is never a labelled `[]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_lanes: Vec<String>,
    /// Why `degraded_lanes` could not run, in one human-readable clause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / vector
    /// search / graph walk + response construction), excluding MCP transport, argument
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
    /// search / graph walk + response construction), excluding MCP transport, argument
    /// deserialization, and response serialization. A first call against a cold server also
    /// includes index warm-up; such responses carry a `notice`. See
    /// [`crate::mcp::helpers::timing`] for the full contract.
    #[serde(default)]
    pub elapsed_us: u64,
}
