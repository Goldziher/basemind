//! What a document search returns (ADR-0029 rules 8 and 9).
//!
//! A search ranks chunks once per query ([`RankedChunk`], kept for the pages that follow), then
//! each page turns its slice of that ranking into [`DocHit`]s: adjacent chunks of one document
//! merged, the neighbours on either side added while the token budget allows, and every path
//! holding the same content named.

use serde::{Deserialize, Serialize};

/// Where a hit's text sits in its document. A field the format has no notion of is `null`:
/// pages for a Markdown file, lines for a PDF.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DocCite {
    /// First page (slide, for a presentation), 1-based.
    pub page_start: Option<u32>,
    pub page_end: Option<u32>,
    /// Heading trail, outermost first.
    pub section: Option<Vec<String>>,
    /// First source line, 1-based; text sources only.
    pub line_start: Option<u32>,
    pub line_end: Option<u32>,
    /// Byte range in the extracted text of the document.
    pub byte_start: u32,
    pub byte_end: u32,
}

/// Which chunks of which document a hit is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocChunkRef {
    /// The document's content key (hex).
    pub doc: String,
    /// First and last chunk ordinal of the hit, before context was added.
    pub ord: u32,
    pub ord_end: u32,
    /// Chunks in the document, when known.
    pub of: Option<u32>,
}

/// The rank a hit held in each lane that ranked it, 1-based.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct LaneRanks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lexical: Option<u32>,
}

/// How many neighbouring chunks a hit's `text` carries on each side.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct DocContext {
    pub before: u32,
    pub after: u32,
}

/// One answer of `docs_search`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocHit {
    /// The first live path holding this content.
    pub path: String,
    /// Other live paths holding the same content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_at: Vec<String>,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub chunk: DocChunkRef,
    pub cite: DocCite,
    /// The hit's chunks, stitched, with its context chunks around them.
    pub text: String,
    pub context: DocContext,
    /// Fused score (reciprocal rank).
    pub score: f32,
    pub lanes: LaneRanks,
    /// Cross-encoder score, when the reranker scored this hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank_score: Option<f32>,
}

/// One chunk of a ranking, as kept between the pages of one query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankedChunk {
    pub doc: String,
    pub ord: u32,
    /// Live paths holding this document, sorted.
    pub paths: Vec<String>,
    pub mime_type: String,
    pub kind: String,
    pub cite: DocCite,
    pub text: String,
    pub score: f32,
    pub lanes: LaneRanks,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank_score: Option<f32>,
}

/// A lane that could not run for a query, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocLaneUnavailable {
    pub lane: String,
    pub reason: String,
}

/// Whether the reranker reordered the head of the ranking.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RerankReport {
    pub ran: bool,
    /// Hits the cross-encoder scored; the rest keep their fused order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidates: Option<u32>,
    /// Why it did not run, when it was asked to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The ranking of one query: what [`super::docs_rank::rank`] computes once and pages from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocsRanking {
    pub query: String,
    pub hits: Vec<RankedChunk>,
    pub lanes_ran: Vec<String>,
    pub lanes_unavailable: Vec<DocLaneUnavailable>,
    pub lexical_fuzzy: bool,
    pub vector_mode: Option<String>,
    pub rerank: RerankReport,
    pub notice: Option<String>,
    /// Set when the liveness projection reached its cap with too few live hits.
    pub truncated_reason: Option<String>,
}

/// One page of a ranking, as `docs_search` answers it.
#[derive(Debug, Serialize, Deserialize)]
pub struct DocsPage {
    pub query: String,
    pub hits: Vec<DocHit>,
    /// Lanes that produced a ranking: `vector`, `lexical`.
    pub lanes_ran: Vec<String>,
    /// Lanes that could not run, each with its reason; the answer is the other lane's alone.
    pub lanes_unavailable: Vec<DocLaneUnavailable>,
    /// The lexical lane was rerun allowing one edit per term, having found less than a page.
    pub lexical_fuzzy: bool,
    /// `exact` when the vector lane ran: every candidate vector was compared.
    pub vector_mode: Option<String>,
    pub rerank: RerankReport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    /// The ranking was cut by a hard bound: `depth` (it goes on past the 100 hits that can be
    /// paged) or `liveness_depth` (the live documents are too many to filter on before ranking, and
    /// the widened search still found too few of them).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<String>,
    /// The token budget ended the page early; `next_cursor` continues.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub budgeted: bool,
    /// Rank the next page starts at, when the ranking goes on.
    #[serde(skip)]
    pub next_offset: Option<u32>,
}
