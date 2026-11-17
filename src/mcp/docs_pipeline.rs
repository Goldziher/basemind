//! `docs_search` (ADR-0029): rank document chunks once per query, then page the ranking.
//!
//! The ranking runs both lanes under one pre-ranking filter, fuses them by reciprocal rank and lets
//! the reranker reorder the head without cutting anything:
//!
//! 1. **Live documents.** The view's document map names, for every path, the `doc_key` its rows
//!    are stored under. A `path` glob and a `keywords` filter narrow that set.
//! 2. **Filter before ranking.** `mime_type`, `kind`, `entity` and the allowed keys are one
//!    predicate both lanes apply. While the allowed keys number at most [`LIVE_KEYS_PREFILTER`]
//!    they go into it; above that, each lane over-fetches and projects onto them, widening until
//!    it has enough or reaches [`LIVE_WIDEN_CAP`], which the response then reports.
//! 3. **Two lanes.** The vector lane (exact nearest neighbours) and the lexical lane (BM25), each
//!    [`LANE_CANDIDATES`] deep. A lexical lane that finds less than a page is rerun allowing one
//!    edit per term. A lane that cannot run is reported with its reason; when neither can, the
//!    call fails.
//! 4. **Fuse** by reciprocal rank (`k = 60`, equal weights) on `(doc_key, chunk_ord)` and cut at
//!    the paging depth.
//! 5. **Rerank** the first [`super::docs_page::RERANK_CANDIDATES`] when the reranker is on; the
//!    rest keep their fused order, and a reranker failure is reported, not raised.

use std::sync::Arc;

use ahash::{AHashMap, AHashSet};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::types_documents::{DocCite, DocLaneUnavailable, DocsRanking, LaneRanks, RankedChunk};
use super::types_tools::DocsSearchParams;
use crate::lance::docs::{DocPredicate, DocStore, MIME_GROUPS, StoreCapabilities};
use crate::lance::docs_schema::{DocChunkHit, SECTION_SEPARATOR};

/// Candidates each lane returns (ADR-0029 rule 4).
pub(super) const LANE_CANDIDATES: usize = 200;
/// Allowed keys sent to the store as a pre-filter; more are projected after ranking. ADR-0029 rule
/// 3 said 20,000; the benchmark measured a 20,000-key list over budget (ADR-0035).
pub(super) const LIVE_KEYS_PREFILTER: usize = 2_000;
/// The most candidates a lane fetches while widening past dead or filtered-out rows.
pub(super) const LIVE_WIDEN_CAP: usize = 4_096;
/// A lexical lane with fewer hits than this is rerun fuzzy: one page.
const LEXICAL_FUZZY_BELOW: usize = super::helpers_page::RANKED_LIMIT_DEFAULT as usize;

const LANE_VECTOR: &str = "vector";
const LANE_LEXICAL: &str = "lexical";

/// The documents of this view: which paths hold each `doc_key`.
#[derive(Debug, Default, Clone)]
pub(super) struct LiveDocs {
    pub by_key: AHashMap<String, Vec<String>>,
}

impl LiveDocs {
    /// Every document of the view's document map whose path passes `path_filter`. The key is the
    /// one the scan stored the rows under: computed from the content hash and the extraction
    /// digest the entry records, never from the current settings.
    pub(super) fn of_store(store: &crate::store::Store, path_filter: Option<&globset::GlobMatcher>) -> Self {
        let mut by_key: AHashMap<String, Vec<String>> = AHashMap::new();
        for (path, entry) in &store.index.doc_files {
            let path = path.to_str_lossy();
            if path_filter.is_some_and(|glob| !glob.is_match(path.as_ref())) {
                continue;
            }
            let key = crate::lance::docs::doc_key(&entry.hash_hex, &entry.config_digest);
            by_key.entry(key).or_default().push(path.into_owned());
        }
        for paths in by_key.values_mut() {
            paths.sort_unstable();
        }
        Self { by_key }
    }
}

/// The filters of one request, checked.
pub(super) struct Filters {
    pub mime_type: Option<String>,
    pub kind: Option<String>,
    pub entity: Option<String>,
    pub keywords: Vec<String>,
    pub path: Option<globset::GlobMatcher>,
}

impl Filters {
    pub(super) fn of(p: &DocsSearchParams) -> Result<Self, McpError> {
        if p.query.trim().is_empty() {
            return Err(McpError::invalid_params("`docs_search`: `query` is empty", None));
        }
        let kind = p.kind.as_deref().map(str::trim).map(str::to_ascii_lowercase);
        if let Some(kind) = &kind
            && !MIME_GROUPS.contains(&kind.as_str())
        {
            return Err(McpError::invalid_params(
                format!(
                    "`docs_search`: unknown `kind` {kind:?}; one of {}",
                    MIME_GROUPS.join(", ")
                ),
                None,
            ));
        }
        if p.entity.is_some()
            && let Some(reason) = crate::extract::doc_tiers::entity_filter_unavailable()
        {
            return Err(McpError::invalid_request(reason, None));
        }
        let path = match p.path.as_deref() {
            Some(pattern) => Some(
                globset::GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .build()
                    .map_err(|error| {
                        McpError::invalid_params(
                            format!("`docs_search`: `path` is not a glob ({error}); e.g. `docs/**/*.md`"),
                            None,
                        )
                    })?
                    .compile_matcher(),
            ),
            None => None,
        };
        Ok(Self {
            mime_type: p.mime_type.clone().filter(|m| !m.trim().is_empty()),
            kind,
            entity: p
                .entity
                .as_deref()
                .map(|e| e.trim().to_lowercase())
                .filter(|e| !e.is_empty()),
            keywords: p
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

/// The retrieval store of this view for the configured embedding preset, or `None` when no scan
/// has written one. Opened without the embedding model: the lexical lane needs no model.
pub(super) async fn docs_store(state: &ServerState) -> Result<Option<DocStore>, McpError> {
    let preset = state.shared.config.documents.embedding_preset.clone();
    let dim = crate::scanner_docs::preset_dim(&preset)
        .map_err(|error| McpError::invalid_request(format!("`docs_search`: {error}"), None))?;
    let parent = state.shared.store.read().await.view_dir.clone();
    tokio::task::spawn_blocking(move || {
        let dir = crate::lance::docs::store_dir(&parent, &preset, dim);
        DocStore::shared_existing(&dir, dim, &preset)
    })
    .await
    .map_err(|e| McpError::internal_error(format!("spawn_blocking: {e}"), None))?
    .map_err(|error| {
        McpError::invalid_request(format!("`docs_search`: the document store did not open: {error}"), None)
    })
}

/// Run `work` against the store on a blocking thread.
async fn on_store<T, F>(store: &DocStore, work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&DocStore) -> anyhow::Result<T> + Send + 'static,
{
    let store = store.clone();
    tokio::task::spawn_blocking(move || work(&store))
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))?
        .map_err(|e| format!("{e:#}"))
}

/// Rank the chunks of the live documents for `p`.
pub(super) async fn rank(
    state: &ServerState,
    p: &DocsSearchParams,
    filters: &Filters,
) -> Result<DocsRanking, McpError> {
    let mut ranking = DocsRanking {
        query: p.query.clone(),
        ..DocsRanking::default()
    };
    let (live, owed) = {
        let store = state.shared.store.read().await;
        (
            LiveDocs::of_store(&store, filters.path.as_ref()),
            store.pending_vectors.docs().len(),
        )
    };
    let Some(store) = docs_store(state).await? else {
        ranking.notice =
            Some("no document has been indexed yet: run `basemind scan` with `[documents] enabled = true`".to_string());
        return Ok(ranking);
    };
    let caps = on_store(&store, DocStore::capabilities)
        .await
        .map_err(|error| McpError::internal_error(format!("`docs_search`: {error}"), None))?;
    if caps.rows == 0 {
        ranking.notice =
            Some("the document store holds no rows yet; a scan in progress may still add them".to_string());
        return Ok(ranking);
    }
    if owed > 0 {
        ranking.notice = Some(format!(
            "{owed} documents are still owed their rows by an interrupted or deferred scan; they are missing from \
             this ranking until the next scan writes them"
        ));
    }

    let mut allowed: AHashSet<String> = live.by_key.keys().cloned().collect();
    if !filters.keywords.is_empty() {
        let words = filters.keywords.clone();
        let with_keywords = on_store(&store, move |store| store.keys_with_keywords(&words))
            .await
            .map_err(|error| McpError::internal_error(format!("`docs_search`: keywords: {error}"), None))?;
        let with_keywords: AHashSet<String> = with_keywords.into_iter().collect();
        allowed.retain(|key| with_keywords.contains(key));
    }
    let predicate = DocPredicate {
        mime_type: filters.mime_type.clone(),
        mime_group: filters.kind.clone(),
        entity: filters.entity.clone(),
        keys: (allowed.len() <= LIVE_KEYS_PREFILTER).then(|| {
            let mut keys: Vec<String> = allowed.iter().cloned().collect();
            keys.sort_unstable();
            keys
        }),
    };
    let allowed = Arc::new(allowed);

    let vector = vector_lane(state, &store, &p.query, &predicate, &allowed, caps).await;
    let lexical = lexical_lane(&store, &p.query, &predicate, &allowed, caps).await;
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
        return Err(McpError::invalid_request(
            format!("`docs_search`: no retrieval lane could run ({})", reasons.join("; ")),
            None,
        ));
    }
    ranking.hits = fuse(&lanes, &live);
    super::docs_page::rerank(state, &p.query, &mut ranking).await;
    Ok(ranking)
}

/// A lane's ranking: its hits, whether it ran fuzzy, and whether the liveness projection gave up
/// at its cap with too few hits.
struct Lane {
    hits: Vec<DocChunkHit>,
    fuzzy: bool,
    capped: bool,
}

async fn vector_lane(
    state: &ServerState,
    store: &DocStore,
    query: &str,
    predicate: &DocPredicate,
    allowed: &Arc<AHashSet<String>>,
    caps: StoreCapabilities,
) -> Result<Lane, String> {
    if !state.shared.config.documents.embed {
        return Err("embeddings are off (`[documents] embed = false`)".to_string());
    }
    if caps.vectors == 0 {
        return Err("no document has a vector yet: the documents were not embedded".to_string());
    }
    let embedding = super::memory::embed_query(state, query)
        .await
        .map_err(|error| format!("the embedding model could not run: {}", error.message))?;
    if embedding.len() != usize::from(store.dim()) {
        return Err(format!(
            "the query vector has {} dimensions, the store {}",
            embedding.len(),
            store.dim()
        ));
    }
    let (predicate, allowed) = (predicate.clone(), Arc::clone(allowed));
    on_store(store, move |store| {
        project(&predicate, &allowed, |k| store.nearest(&embedding, &predicate, k))
    })
    .await
    .map(|(hits, capped)| Lane {
        hits,
        fuzzy: false,
        capped,
    })
}

async fn lexical_lane(
    store: &DocStore,
    query: &str,
    predicate: &DocPredicate,
    allowed: &Arc<AHashSet<String>>,
    caps: StoreCapabilities,
) -> Result<Lane, String> {
    if !caps.lexical {
        return Err(
            "the full-text index is not built yet; the scan that wrote the rows builds it as it ends".to_string(),
        );
    }
    let (text, predicate, allowed) = (query.to_string(), predicate.clone(), Arc::clone(allowed));
    on_store(store, move |store| {
        let (hits, capped) = project(&predicate, &allowed, |k| store.lexical(&text, &predicate, k, false))?;
        if hits.len() >= LEXICAL_FUZZY_BELOW {
            return Ok(Lane {
                hits,
                fuzzy: false,
                capped,
            });
        }
        match project(&predicate, &allowed, |k| store.lexical(&text, &predicate, k, true)) {
            Ok((fuzzy_hits, fuzzy_capped)) => Ok(Lane {
                hits: fuzzy_hits,
                fuzzy: true,
                capped: fuzzy_capped,
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
    })
    .await
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

/// Reciprocal-rank fusion of the lanes on `(doc_key, chunk_ord)`, cut at the paging depth.
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
        .take(super::helpers_page::RANKED_FETCH as usize)
        .collect()
}

fn chunk_id(hit: &DocChunkHit) -> String {
    format!("{}:{}", hit.doc_key, hit.chunk_ord)
}

/// The citation of one stored chunk.
pub(super) fn cite_of(hit: &DocChunkHit) -> DocCite {
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
    }
}

/// `docs_search`: the ranking of the query (computed once per index generation and kept), and the
/// page of it the cursor points at.
pub(super) async fn docs_search(state: &ServerState, p: DocsSearchParams) -> Result<CallToolResult, McpError> {
    use super::helpers_page::{RANKED_LIMIT_DEFAULT, ranked_offset};

    let started = std::time::Instant::now();
    let generation = state.shared.index_generation.load(std::sync::atomic::Ordering::Relaxed);
    let offset = ranked_offset("docs_search", p.cursor.as_ref(), generation)?;
    let limit = p.limit.unwrap_or(RANKED_LIMIT_DEFAULT);
    let filters = Filters::of(&p)?;
    let key = super::ranked_cache::RankedCache::key(
        "docs_search",
        &serde_json::json!({
            "query": &p.query,
            "mime_type": &p.mime_type,
            "kind": &filters.kind,
            "entity": &filters.entity,
            "keywords": &filters.keywords,
            "path": &p.path,
        }),
    );
    let cache = &state.shared.ranked;
    let ranking = match cache
        .get(&key, generation)
        .and_then(|body| serde_json::from_value::<DocsRanking>(body).ok())
    {
        Some(ranking) => ranking,
        None => {
            let ranking = rank(state, &p, &filters).await?;
            if is_cacheable(&ranking)
                && let Ok(body) = serde_json::to_value(&ranking)
            {
                cache.put(key, generation, &body);
            }
            ranking
        }
    };
    let page = super::docs_page::page(
        state,
        ranking,
        offset,
        limit,
        super::tool_contract::budget(p.max_tokens),
        generation,
    )
    .await?;
    let mut page = page;
    page.elapsed_us = super::helpers::elapsed_us(started);
    super::helpers::json_result(&page)
}

/// A degraded or empty answer is not kept: it is the one most likely to differ a moment later
/// (ADR-0029 rule 9).
fn is_cacheable(ranking: &DocsRanking) -> bool {
    !ranking.hits.is_empty()
        && ranking.lanes_unavailable.is_empty()
        && ranking.rerank.reason.is_none()
        && ranking.notice.is_none()
}

/// Whether `ranking` reports the reranker failing, for tests.
#[cfg(test)]
pub(super) fn rerank_failed(report: &super::types_documents::RerankReport) -> bool {
    !report.ran && report.reason.is_some()
}

#[cfg(test)]
#[path = "docs_pipeline_tests.rs"]
mod tests;
