//! Rank document chunks for one query (ADR-0029 rules 3 to 7, as amended by ADR-0037).
//!
//! 1. **Live documents.** The view's document map names, for every path, the `doc_key` its rows
//!    are stored under. A `path` filter (a prefix, or a glob when it holds `*`) and a `keywords`
//!    filter narrow that set.
//! 2. **Filter before ranking.** `mime_type`, `format`, `entity` and the allowed keys are one
//!    predicate both lanes apply. While the allowed keys number at most [`LIVE_KEYS_PREFILTER`]
//!    they go into it; above that, each lane over-fetches and projects onto them, widening until
//!    it has enough or reaches [`LIVE_WIDEN_CAP`], which the ranking then reports.
//! 3. **Two lanes.** The vector lane (exact nearest neighbours) and the lexical lane (BM25), each
//!    [`LANE_CANDIDATES`] deep. A lexical lane that finds less than a page is rerun allowing one
//!    edit per term. A lane that cannot run is reported with its reason; when neither can, the
//!    search fails.
//! 4. **Fuse** by reciprocal rank (`k = 60`, equal weights) on `(doc_key, chunk_ord)` and cut at
//!    [`RANKED_FETCH`].
//! 5. **Rerank** the first [`RERANK_CANDIDATES`] when asked; the rest keep their fused order, and a
//!    reranker failure is reported on the ranking, not raised.
//!
//! Everything here is synchronous and independent of the MCP layer: the caller supplies the
//! store, the live documents and a query embedder, and maps [`DocsSearchError`] to its own errors.

use ahash::{AHashMap, AHashSet};

use super::docs::{DocPredicate, DocStore, MIME_GROUPS, StoreCapabilities};
use super::docs_schema::{DocChunkHit, SECTION_SEPARATOR};
use super::docs_types::{DocCite, DocLaneUnavailable, DocsRanking, LaneRanks, RankedChunk, RerankReport};

/// Candidates each lane returns (ADR-0029 rule 4).
pub const LANE_CANDIDATES: usize = 200;
/// Allowed keys sent to the store as a pre-filter; more are projected after ranking (ADR-0037).
pub const LIVE_KEYS_PREFILTER: usize = 2_000;
/// The most candidates a lane fetches while widening past dead or filtered-out rows.
pub const LIVE_WIDEN_CAP: usize = 4_096;
/// Hits of a ranking that can be paged through, plus one that says whether it goes on.
pub const RANKED_FETCH: usize = 101;
/// A lexical lane with fewer hits than this, one page, is rerun fuzzy.
const LEXICAL_FUZZY_BELOW: usize = 10;
/// Fused hits the reranker scores, at most (ADR-0029 rule 7).
pub const RERANK_CANDIDATES: usize = 50;

const LANE_VECTOR: &str = "vector";
const LANE_LEXICAL: &str = "lexical";

/// Why a document search could not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocsSearchError {
    /// An argument the caller can fix; the text names the fix.
    InvalidArgument(String),
    /// A filter this build cannot apply.
    Unsupported(String),
    /// Neither lane could run; the text gives each lane's reason.
    NoLane(String),
    /// The store failed.
    Store(String),
}

impl std::fmt::Display for DocsSearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArgument(text) | Self::Unsupported(text) | Self::NoLane(text) | Self::Store(text) => {
                f.write_str(text)
            }
        }
    }
}

impl std::error::Error for DocsSearchError {}

/// What a caller asks for, before it is checked.
#[derive(Debug, Clone, Default)]
pub struct DocsRequest {
    pub query: String,
    pub mime_type: Option<String>,
    /// The document format family, one of [`MIME_GROUPS`].
    pub format: Option<String>,
    pub entity: Option<String>,
    pub keywords: Option<String>,
    pub path: Option<String>,
}

/// The filters of one request, checked.
pub struct Filters {
    pub mime_type: Option<String>,
    pub format: Option<String>,
    pub entity: Option<String>,
    pub keywords: Vec<String>,
    pub path: Option<crate::path_filter::PathFilter>,
}

impl Filters {
    /// Check `request`. `entity_unavailable` is why this build cannot filter on entities, if it
    /// cannot.
    pub fn parse(request: &DocsRequest, entity_unavailable: Option<String>) -> Result<Self, DocsSearchError> {
        if request.query.trim().is_empty() {
            return Err(DocsSearchError::InvalidArgument("`query` is empty".to_string()));
        }
        let format = request.format.as_deref().map(str::trim).map(str::to_ascii_lowercase);
        if let Some(format) = &format
            && !MIME_GROUPS.contains(&format.as_str())
        {
            return Err(DocsSearchError::InvalidArgument(format!(
                "unknown `format` {format:?}; one of {}",
                MIME_GROUPS.join(", ")
            )));
        }
        if request.entity.is_some()
            && let Some(reason) = entity_unavailable
        {
            return Err(DocsSearchError::Unsupported(reason));
        }
        let path = request
            .path
            .as_deref()
            .map(crate::path_filter::PathFilter::parse)
            .transpose()
            .map_err(DocsSearchError::InvalidArgument)?;
        Ok(Self {
            mime_type: request.mime_type.clone().filter(|m| !m.trim().is_empty()),
            format,
            entity: request
                .entity
                .as_deref()
                .map(|e| e.trim().to_lowercase())
                .filter(|e| !e.is_empty()),
            keywords: request
                .keywords
                .as_deref()
                .map(|k| {
                    k.split(',')
                        .map(|w| w.trim().to_lowercase())
                        .filter(|w| !w.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            path,
        })
    }
}

/// The documents of this view: which paths hold each `doc_key`.
#[derive(Debug, Default, Clone)]
pub struct LiveDocs {
    pub by_key: AHashMap<String, Vec<String>>,
}

impl LiveDocs {
    /// Every document of the view's document map whose path passes `path_filter`. The key is the
    /// one the scan stored the rows under: computed from the content hash and the extraction
    /// digest the entry records, never from the current settings.
    pub fn of_store(store: &crate::store::Store, path_filter: Option<&crate::path_filter::PathFilter>) -> Self {
        let mut by_key: AHashMap<String, Vec<String>> = AHashMap::new();
        for (path, entry) in &store.index.doc_files {
            if path_filter.is_some_and(|filter| !filter.matches(path.as_bytes())) {
                continue;
            }
            let path = path.to_str_lossy();
            let key = super::docs::doc_key(&entry.hash_hex, &entry.config_digest);
            by_key.entry(key).or_default().push(path.into_owned());
        }
        for paths in by_key.values_mut() {
            paths.sort_unstable();
        }
        Self { by_key }
    }
}

/// How the vector lane gets its query vector: `Ok` the vector, `Err` why it cannot.
pub type QueryEmbedder<'a> = &'a dyn Fn(&str) -> Result<Vec<f32>, String>;

/// The reranker to run, when it is on.
#[derive(Debug, Clone)]
pub struct RerankSettings {
    pub preset: String,
    /// How many fused hits it scores, at most [`RERANK_CANDIDATES`].
    pub candidates: usize,
    pub acceleration: Option<xberg::AccelerationConfig>,
}

/// What [`rank`] works over.
pub struct RankInputs<'a> {
    /// The view's store; `None` when no scan has written one.
    pub store: Option<&'a DocStore>,
    pub live: LiveDocs,
    /// Paths still owed their rows by an interrupted or deferred scan.
    pub owed: usize,
    /// The vector lane's query embedder, or why the lane is off.
    pub embed: Result<QueryEmbedder<'a>, String>,
    pub rerank: Option<RerankSettings>,
}

/// Rank the chunks of the live documents for `query` under `filters`.
pub fn rank(query: &str, filters: &Filters, inputs: RankInputs<'_>) -> Result<DocsRanking, DocsSearchError> {
    let mut ranking = DocsRanking {
        query: query.to_string(),
        ..DocsRanking::default()
    };
    let Some(store) = inputs.store else {
        ranking.notice =
            Some("no document has been indexed yet: run `basemind scan` with `[documents] enabled = true`".to_string());
        return Ok(ranking);
    };
    let caps = store
        .capabilities()
        .map_err(|error| DocsSearchError::Store(format!("{error:#}")))?;
    if caps.rows == 0 {
        ranking.notice =
            Some("the document store holds no rows yet; a scan in progress may still add them".to_string());
        return Ok(ranking);
    }
    if inputs.owed > 0 {
        ranking.notice = Some(format!(
            "{} documents are still owed their rows by an interrupted or deferred scan; they are missing from this \
             ranking until the next scan writes them",
            inputs.owed
        ));
    }

    let live = inputs.live;
    let mut allowed: AHashSet<String> = live.by_key.keys().cloned().collect();
    if !filters.keywords.is_empty() {
        let with_keywords: AHashSet<String> = store
            .keys_with_keywords(&filters.keywords)
            .map_err(|error| DocsSearchError::Store(format!("keywords: {error:#}")))?
            .into_iter()
            .collect();
        allowed.retain(|key| with_keywords.contains(key));
    }
    let predicate = DocPredicate {
        mime_type: filters.mime_type.clone(),
        mime_group: filters.format.clone(),
        entity: filters.entity.clone(),
        keys: (allowed.len() <= LIVE_KEYS_PREFILTER).then(|| {
            let mut keys: Vec<String> = allowed.iter().cloned().collect();
            keys.sort_unstable();
            keys
        }),
    };

    let vector = vector_lane(store, query, &predicate, &allowed, caps, inputs.embed);
    let lexical = lexical_lane(store, query, &predicate, &allowed, caps);
    let mut lanes: Vec<(&'static str, Vec<DocChunkHit>)> = Vec::new();
    for (name, outcome) in [(LANE_VECTOR, vector), (LANE_LEXICAL, lexical)] {
        match outcome {
            Ok(lane) => {
                ranking.lanes_ran.push(name.to_string());
                ranking.lexical_fuzzy |= lane.fuzzy;
                if lane.capped {
                    ranking.truncated_reason = Some("liveness_depth".to_string());
                }
                if name == LANE_VECTOR {
                    ranking.vector_mode = Some("exact".to_string());
                }
                lanes.push((name, lane.hits));
            }
            Err(reason) => ranking.lanes_unavailable.push(DocLaneUnavailable {
                lane: name.to_string(),
                reason,
            }),
        }
    }
    if lanes.is_empty() {
        let reasons: Vec<String> = ranking
            .lanes_unavailable
            .iter()
            .map(|lane| format!("{}: {}", lane.lane, lane.reason))
            .collect();
        return Err(DocsSearchError::NoLane(format!(
            "no retrieval lane could run ({})",
            reasons.join("; ")
        )));
    }
    ranking.hits = fuse(&lanes, &live);
    if let Some(settings) = &inputs.rerank {
        rerank(query, settings, &mut ranking);
    }
    Ok(ranking)
}

/// A lane's ranking: its hits, whether it ran fuzzy, and whether the liveness projection gave up
/// at its cap with too few hits.
struct Lane {
    hits: Vec<DocChunkHit>,
    fuzzy: bool,
    capped: bool,
}

fn vector_lane(
    store: &DocStore,
    query: &str,
    predicate: &DocPredicate,
    allowed: &AHashSet<String>,
    caps: StoreCapabilities,
    embed: Result<QueryEmbedder<'_>, String>,
) -> Result<Lane, String> {
    let embed = embed?;
    if caps.vectors == 0 {
        return Err("no document has a vector yet: the documents were not embedded".to_string());
    }
    let embedding = embed(query).map_err(|error| format!("the embedding model could not run: {error}"))?;
    if embedding.len() != usize::from(store.dim()) {
        return Err(format!(
            "the query vector has {} dimensions, the store {}",
            embedding.len(),
            store.dim()
        ));
    }
    project(predicate, allowed, |k| store.nearest(&embedding, predicate, k))
        .map(|(hits, capped)| Lane {
            hits,
            fuzzy: false,
            capped,
        })
        .map_err(|error| format!("{error:#}"))
}

fn lexical_lane(
    store: &DocStore,
    query: &str,
    predicate: &DocPredicate,
    allowed: &AHashSet<String>,
    caps: StoreCapabilities,
) -> Result<Lane, String> {
    if !caps.lexical {
        return Err(
            "the full-text index is not built yet; the scan that wrote the rows builds it as it ends".to_string(),
        );
    }
    let (hits, capped) = project(predicate, allowed, |k| store.lexical(query, predicate, k, false))
        .map_err(|error| format!("{error:#}"))?;
    if hits.len() >= LEXICAL_FUZZY_BELOW {
        return Ok(Lane {
            hits,
            fuzzy: false,
            capped,
        });
    }
    match project(predicate, allowed, |k| store.lexical(query, predicate, k, true)) {
        // The fuzzy query matches unstemmed terms, so it can find fewer chunks than the exact one
        // (`reconciled` is indexed as `reconcil`, two edits away); it only ever widens the answer.
        Ok((fuzzy_hits, fuzzy_capped)) if fuzzy_hits.len() > hits.len() => Ok(Lane {
            hits: fuzzy_hits,
            fuzzy: true,
            capped: fuzzy_capped,
        }),
        Ok(_) => Ok(Lane {
            hits,
            fuzzy: false,
            capped,
        }),
        Err(error) => {
            tracing::debug!(%error, "docs_search: the fuzzy lexical rerun failed; keeping the exact hits");
            Ok(Lane {
                hits,
                fuzzy: false,
                capped,
            })
        }
    }
}

/// Run `search` for [`LANE_CANDIDATES`] hits of live documents. With the allowed keys in the
/// predicate the store already applied them; otherwise fetch more, keep the allowed ones and widen
/// (3x, doubling, up to [`LIVE_WIDEN_CAP`]) until there are enough or the store has no more.
/// Returns the hits and whether the cap was reached short of enough.
fn project(
    predicate: &DocPredicate,
    allowed: &AHashSet<String>,
    mut search: impl FnMut(usize) -> anyhow::Result<Vec<DocChunkHit>>,
) -> anyhow::Result<(Vec<DocChunkHit>, bool)> {
    if predicate.keys.is_some() {
        return Ok((search(LANE_CANDIDATES)?, false));
    }
    let mut fetch = (LANE_CANDIDATES * 3).max(LANE_CANDIDATES + 64);
    loop {
        let found = search(fetch)?;
        let exhausted = found.len() < fetch;
        let mut kept: Vec<DocChunkHit> = found.into_iter().filter(|hit| allowed.contains(&hit.doc_key)).collect();
        if kept.len() >= LANE_CANDIDATES || exhausted {
            kept.truncate(LANE_CANDIDATES);
            return Ok((kept, false));
        }
        if fetch >= LIVE_WIDEN_CAP {
            return Ok((kept, true));
        }
        fetch = (fetch * 2).min(LIVE_WIDEN_CAP);
    }
}

/// Reciprocal-rank fusion of the lanes on `(doc_key, chunk_ord)`, cut at [`RANKED_FETCH`].
fn fuse(lanes: &[(&'static str, Vec<DocChunkHit>)], live: &LiveDocs) -> Vec<RankedChunk> {
    let ids: Vec<(&'static str, Vec<String>)> = lanes
        .iter()
        .map(|(name, hits)| (*name, hits.iter().map(chunk_id).collect()))
        .collect();
    let fusion: Vec<crate::search::rrf::FusionLane<'_>> = ids
        .iter()
        .map(|(name, ids)| crate::search::rrf::FusionLane::new(name, ids, 1.0))
        .collect();
    let mut by_id: AHashMap<String, &DocChunkHit> = AHashMap::new();
    for (_, hits) in lanes {
        for hit in hits {
            by_id.entry(chunk_id(hit)).or_insert(hit);
        }
    }
    crate::search::rrf::rrf_fuse_detailed(&fusion, crate::search::rrf::DEFAULT_RRF_K)
        .into_iter()
        .filter_map(|fused| {
            let hit = by_id.get(&fused.chunk_id)?;
            let paths = live.by_key.get(&hit.doc_key)?.clone();
            let mut lanes = LaneRanks::default();
            for (lane, rank) in fused.lane_ranks {
                match lane {
                    LANE_VECTOR => lanes.vector = Some(rank),
                    _ => lanes.lexical = Some(rank),
                }
            }
            Some(RankedChunk {
                doc: hit.doc_key.clone(),
                ord: hit.chunk_ord,
                paths,
                mime_type: hit.mime_type.clone(),
                kind: hit.chunk_kind.clone(),
                cite: cite_of(hit),
                text: hit.text.clone(),
                score: fused.score,
                lanes,
                rerank_score: None,
            })
        })
        .take(RANKED_FETCH)
        .collect()
}

fn chunk_id(hit: &DocChunkHit) -> String {
    format!("{}:{}", hit.doc_key, hit.chunk_ord)
}

/// The citation of one stored chunk.
pub fn cite_of(hit: &DocChunkHit) -> DocCite {
    DocCite {
        page_start: hit.page_start,
        page_end: hit.page_end,
        section: hit
            .section
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.split(SECTION_SEPARATOR).map(str::to_string).collect()),
        line_start: hit.line_start,
        line_end: hit.line_end,
        byte_start: hit.byte_start,
        byte_end: hit.byte_end,
        headings: Vec::new(),
    }
}

/// Reorder the first hits by the cross-encoder. Nothing is dropped, and a failure leaves the fused
/// order with the reason on the report.
fn rerank(query: &str, settings: &RerankSettings, ranking: &mut DocsRanking) {
    if ranking.hits.is_empty() {
        return;
    }
    let candidates = ranking
        .hits
        .len()
        .min(RERANK_CANDIDATES)
        .min(settings.candidates.max(1));
    let fail = |reason: String| RerankReport {
        ran: false,
        candidates: None,
        reason: Some(reason),
    };
    if xberg::get_reranker_preset(&settings.preset).is_none() {
        ranking.rerank = fail(format!("unknown reranker preset {:?}", settings.preset));
        return;
    }
    let documents: Vec<String> = ranking.hits[..candidates]
        .iter()
        .map(|hit| match &hit.cite.section {
            Some(section) if !section.is_empty() => format!("{}\n{}", section.join(" > "), hit.text),
            _ => hit.text.clone(),
        })
        .collect();
    let config = xberg::core::config::RerankerConfig {
        model: xberg::core::config::RerankerModelType::Preset {
            name: settings.preset.clone(),
        },
        top_k: Some(candidates),
        acceleration: settings.acceleration.clone(),
        ..Default::default()
    };
    match crate::embeddings::rerank(query.to_string(), documents, &config, &settings.preset) {
        Ok(scored) => match reorder(&mut ranking.hits, candidates, &scored) {
            Ok(()) => {
                ranking.rerank = RerankReport {
                    ran: true,
                    candidates: u32::try_from(candidates).ok(),
                    reason: None,
                }
            }
            Err(reason) => ranking.rerank = fail(reason),
        },
        Err(error) => ranking.rerank = fail(format!("the reranker failed: {error}")),
    }
}

/// Put the first `candidates` hits in the order of `scored`, each scored hit carrying its score. A
/// hit the reranker left out keeps its place after the scored ones. An answer naming a hit out of
/// range is refused before anything moves, so the hits keep their fused order and no rerank score.
fn reorder(hits: &mut Vec<RankedChunk>, candidates: usize, scored: &[xberg::RerankedDocument]) -> Result<(), String> {
    if let Some(bad) = scored.iter().find(|result| result.index >= candidates) {
        return Err(format!(
            "the reranker returned index {} of {candidates} candidates",
            bad.index
        ));
    }
    let mut head: Vec<Option<RankedChunk>> = hits.drain(..candidates).map(Some).collect();
    let mut reordered = Vec::with_capacity(candidates);
    for result in scored {
        if let Some(mut hit) = head[result.index].take() {
            hit.rerank_score = Some(result.score);
            reordered.push(hit);
        }
    }
    reordered.extend(head.into_iter().flatten());
    hits.splice(0..0, reordered);
    Ok(())
}

/// Whether `ranking` may be kept for the pages and repeats that follow: a degraded or empty answer
/// is the one most likely to differ a moment later (ADR-0029 rule 9).
pub fn is_cacheable(ranking: &DocsRanking) -> bool {
    !ranking.hits.is_empty()
        && ranking.lanes_unavailable.is_empty()
        && ranking.rerank.reason.is_none()
        && ranking.notice.is_none()
}

#[cfg(test)]
#[path = "docs_rank_tests.rs"]
mod tests;
