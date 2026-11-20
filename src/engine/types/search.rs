//! Types of `code_search`, of `basemind code chunk` (the fetch half of a search hit) and of
//! `docs_search`.

use serde::{Deserialize, Serialize};

#[cfg(any(feature = "code-search", feature = "documents"))]
use super::LifecycleNotice;
use crate::engine::cursor::Cursor;
use crate::path::RelPath;

/// Arguments of `code_search`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeSearchParams {
    /// What the code does, in plain words.
    pub query: String,
    /// Retrieval lane (default `hybrid`).
    #[serde(default)]
    #[schemars(extend("enum" = ["hybrid", "semantic", "keyword"]))]
    pub lane: Option<String>,
    /// Prefix, or a glob when it has `*`, e.g. `src/`.
    #[serde(default)]
    pub path: Option<String>,
    /// Language, e.g. `rust`.
    #[serde(default)]
    pub language: Option<String>,
    /// Default 10.
    #[serde(default)]
    pub limit: Option<u32>,
    /// A prior `next_cursor`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Default 6000.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Files changed since rev's merge base (empty: HEAD), A..B or A...B; main...HEAD+importers adds importers.
    #[serde(default)]
    pub changed: Option<String>,
}

/// Arguments of `docs_search`.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocsSearchParams {
    /// What to look for, in plain words.
    pub query: String,
    /// Exact MIME filter, e.g. `application/pdf`.
    #[serde(default)]
    pub mime_type: Option<String>,
    /// pdf, office, html, markdown, email, notebook, text or other.
    #[serde(default)]
    pub format: Option<String>,
    /// Prefix, or a glob when it has `*`, e.g. `src/`.
    #[serde(default)]
    pub path: Option<String>,
    /// Whole keywords, comma-separated; any one matches.
    #[serde(default)]
    pub keywords: Option<String>,
    /// Keep chunks naming an entity of this category, e.g. `PERSON`.
    #[serde(default)]
    pub entity: Option<String>,
    /// Default 10.
    #[serde(default)]
    pub limit: Option<u32>,
    /// A prior `next_cursor`.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// Default 6000.
    #[serde(default)]
    pub max_tokens: Option<u32>,
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
    /// The first (sorted) indexed path holding this chunk's content now.
    pub path: String,
    /// Other indexed paths with the same content (copies of the file), so they are one hit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_at: Vec<String>,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LaneUnavailable {
    pub lane: String,
    pub reason: String,
}

/// The ranked retrieval of one `code_search` query, which every page of it is cut from: the hits to
/// the paging depth (and one more), and what the ranking says about itself.
#[cfg(feature = "code-search")]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CodeSearchRanking {
    pub query: String,
    /// `stale_chunks` when ranked chunks no indexed file holds any more were dropped.
    pub truncated_reason: Option<String>,
    /// Ranked chunks dropped because no indexed file holds their content any more.
    pub omitted: usize,
    pub hits: Vec<CodeSearchHit>,
    /// Lanes that produced a ranking for this query: `vector`, `keyword`, `exact`.
    pub lanes_ran: Vec<String>,
    /// Lanes that did NOT run, each with why. Non-empty means `hits` is a PARTIAL result from the
    /// lanes that did run: a caller must be able to tell "searched and found nothing" from "could
    /// not search". When NO lane runs the call is an error instead, so this is never a labelled
    /// `[]`.
    pub lanes_unavailable: Vec<LaneUnavailable>,
}

/// One page of a `code_search` ranking.
///
/// Serialized by hand, in the key order the response has always had: the ranking's own fields
/// (with a `stale_chunks` cut among them), the timing, the lanes, then the page and its cuts, and
/// last the lifecycle `notice`.
#[cfg(feature = "code-search")]
#[derive(Debug)]
pub(crate) struct CodeSearchPage {
    pub ranking: CodeSearchRanking,
    /// Files the `changed` scope holds, when one was passed: `0` is a change that touched nothing,
    /// told apart from one with no hit.
    pub scope_files: Option<usize>,
    /// True when a `max_tokens` budget dropped trailing `hits`.
    pub budgeted: bool,
    /// True when the ranking goes on past the paging depth (`truncated_reason: depth`).
    pub depth_cut: bool,
    pub next_cursor: Option<Cursor>,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / vector
    /// search + response construction), excluding MCP transport, argument
    /// deserialization, and response serialization. See [`crate::engine::timing`] for the full
    /// contract.
    pub elapsed_us: u64,
    /// Lifecycle notice while the index is incomplete (warming, building, rescanning, rebuilding or
    /// empty), as on every verb tool; absent when it is ready. Computed per call, never kept with
    /// the ranking.
    pub notice: Option<LifecycleNotice>,
}

#[cfg(feature = "code-search")]
impl Serialize for CodeSearchPage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let ranking = &self.ranking;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("query", &ranking.query)?;
        if let Some(scope_files) = self.scope_files {
            map.serialize_entry("scope_files", &scope_files)?;
        }
        if let Some(reason) = &ranking.truncated_reason {
            map.serialize_entry("truncated", &true)?;
            map.serialize_entry("truncated_reason", reason)?;
        }
        if ranking.omitted > 0 {
            map.serialize_entry("omitted", &ranking.omitted)?;
        }
        map.serialize_entry("elapsed_us", &self.elapsed_us)?;
        map.serialize_entry("lanes_ran", &ranking.lanes_ran)?;
        if !ranking.lanes_unavailable.is_empty() {
            map.serialize_entry("lanes_unavailable", &ranking.lanes_unavailable)?;
        }
        map.serialize_entry("hits", &ranking.hits)?;
        if self.budgeted {
            map.serialize_entry("budgeted", &true)?;
        }
        if self.depth_cut && ranking.truncated_reason.is_none() {
            map.serialize_entry("truncated", &true)?;
            map.serialize_entry("truncated_reason", super::super::ranked::REASON_DEPTH)?;
        }
        if let Some(cursor) = &self.next_cursor {
            map.serialize_entry("next_cursor", cursor)?;
        }
        if let Some(notice) = &self.notice {
            map.serialize_entry("notice", notice)?;
        }
        map.end()
    }
}

/// Response for `get_chunk` — the full chunk body plus its metadata.
#[cfg(feature = "code-search")]
#[derive(Debug, Serialize)]
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
    /// Server-side handler latency in microseconds. See [`crate::engine::timing`].
    #[serde(default)]
    pub elapsed_us: u64,
}

/// The response of `docs_search`: a page of the document search engine
/// ([`crate::lance::docs_page`]) plus the paging and timing fields.
#[cfg(feature = "documents")]
#[derive(Debug, Serialize)]
pub(crate) struct DocsSearchResponse {
    #[serde(flatten)]
    pub page: crate::lance::docs_types::DocsPage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    pub elapsed_us: u64,
    /// Lifecycle notice while the index is incomplete, as on every verb tool; absent when it is
    /// ready. It takes the place of the page's own document `notice`, so a response has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
}
