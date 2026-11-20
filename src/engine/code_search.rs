//! The retrieval lanes of `code_search`.
//!
//! [`retrieve`] dispatches by `lane`: `hybrid` (default) fuses the vector, keyword, and exact-symbol
//! lanes via RRF ([`hybrid_hits`]); `semantic` ([`semantic_hits`]) is vector KNN over the LanceDB
//! `code_chunks` table; `keyword` ([`keyword_hits`]) is native BM25 over the Fjall index. An
//! optional cross-encoder [`rerank_hits`] pass reorders the result. The fetch half,
//! `basemind code chunk`, reads the file's content-addressed `.chunk` sidecar and returns one
//! chunk's body, no LanceDB round-trip (`ops::code_search`).

#![cfg(feature = "code-search")]

use ahash::AHashSet;

use super::filters::Filters;
use super::scope::ChangeScope;
use super::types::search::{CodeSearchHit, CodeSearchRanking, LaneUnavailable};
use super::{EngineError, Workspace};
use crate::path::RelPath;
use crate::search::bm25::bm25_search;
use crate::search::exact::exact_lane_chunk_ids;
use crate::search::hydrate::ChunkHydrator;
use crate::search::lanes::{Degraded, LaneCause, LaneOutcome};
use crate::search::rrf::{
    DEFAULT_RRF_K, FusionLane, LANE_EXACT, LANE_KEYWORD, LANE_VECTOR, WEIGHT_EXACT, WEIGHT_KEYWORD, WEIGHT_VECTOR,
    rrf_fuse_detailed,
};
use crate::store::Store;

/// Hydrated hits between two checks of the caller's cancellation.
const HYDRATE_CANCEL_EVERY: usize = 16;

/// [`EngineError::Cancelled`] once `cancelled` says so.
fn stop_if(cancelled: &dyn Fn() -> bool) -> Result<(), EngineError> {
    if cancelled() {
        Err(EngineError::Cancelled)
    } else {
        Ok(())
    }
}

/// Candidates each lane ranks when a `path` or `language` filter is set: the filter keeps the hits
/// it passes among them.
const FILTERED_LANE_DEPTH: usize = 1_000;

/// How many more chunks the lanes rank when a `changed` scope will drop the ones outside it.
const SCOPED_OVERFETCH: usize = 8;

/// The ranking of `query` over `lane` (default `hybrid`), at most `limit` hits after hydration, each
/// in a file `filters` passes. With a `scope`, the lanes rank [`SCOPED_OVERFETCH`] times as deep and
/// only the hits in its files are kept, so a change's chunks ranked below that depth are not found.
/// Stops with [`EngineError::Cancelled`] once `cancelled` says so, checked between the lanes, every
/// [`HYDRATE_CANCEL_EVERY`] hydrated hits and before the rerank: it runs on a blocking thread that
/// abandoning the call does not stop.
pub(super) fn retrieve(
    ws: &Workspace,
    query: String,
    lane: Option<&str>,
    limit: usize,
    filters: &Filters<'_>,
    scope: Option<&ChangeScope>,
    cancelled: &dyn Fn() -> bool,
) -> Result<CodeSearchRanking, EngineError> {
    stop_if(cancelled)?;
    let rr = &ws.config.code_search.reranker;
    let (rerank_enabled, rerank_top_k) = (rr.enabled, rr.top_k);
    let fetch_n = if rerank_enabled { limit.max(rerank_top_k) } else { limit };
    let mut depth = if filters.is_empty() {
        fetch_n
    } else {
        FILTERED_LANE_DEPTH.max(fetch_n)
    };
    if scope.is_some() {
        depth = depth.max(fetch_n.saturating_mul(SCOPED_OVERFETCH));
    }

    let mode = lane.map(str::trim).unwrap_or("hybrid");
    let (seeds, report): (Vec<CodeSearchHit>, LaneReport) = if mode.is_empty() || mode.eq_ignore_ascii_case("hybrid") {
        hybrid_hits(ws, &query, depth, cancelled)?
    } else if mode.eq_ignore_ascii_case("semantic") {
        (
            semantic_hits(ws, &query, depth, SEMANTIC_MEANWHILE)?,
            LaneReport::ran_only("vector"),
        )
    } else if mode.eq_ignore_ascii_case("keyword") {
        (keyword_hits(ws, &query, depth)?, LaneReport::ran_only("keyword"))
    } else {
        return Err(EngineError::unavailable(format!(
            "`code_search`: unknown lane {mode:?}; expected \"hybrid\", \"semantic\", or \"keyword\""
        )));
    };

    let keep_text = if rerank_enabled {
        rerank_top_k.clamp(1, RERANK_CANDIDATES)
    } else {
        0
    };
    stop_if(cancelled)?;
    let hydrated = hydrate_hits(&ws.store_read(), seeds, fetch_n, keep_text, filters, scope, cancelled)?;

    // The reranker reorders the head of the ranking and drops nothing; a reranker that cannot run
    // leaves the fused order and says why, as a lane that did not run.
    let mut report = report;
    let hits = if rerank_enabled {
        stop_if(cancelled)?;
        let (hits, failure) = rerank_hits(ws, &query, hydrated.hits, hydrated.texts, &rr.preset);
        if let Some(reason) = failure {
            report.unavailable.push(LaneUnavailable {
                lane: "rerank".to_string(),
                reason,
            });
        }
        hits
    } else {
        hydrated.hits
    };
    Ok(CodeSearchRanking {
        query,
        truncated_reason: (hydrated.omitted > 0).then(|| "stale_chunks".to_string()),
        omitted: hydrated.omitted,
        hits,
        lanes_ran: report.ran.into_iter().map(str::to_string).collect(),
        lanes_unavailable: report.unavailable,
    })
}

/// Why a hybrid search ran without its vector lane when `[code_search] embed` is off.
const EMBEDDINGS_OFF: &str = "embeddings are off: set `[code_search] embed = true` and run `index_rescan`";

/// Whether `ranking` may be kept for the pages and repeats that follow: a degraded or empty answer
/// is the one most likely to differ a moment later. A vector lane the configuration turned off is
/// not degraded: turning it on takes an `index_rescan`, which drops the kept rankings.
pub(super) fn is_cacheable(ranking: &CodeSearchRanking) -> bool {
    !ranking.hits.is_empty()
        && ranking
            .lanes_unavailable
            .iter()
            .all(|unavailable| unavailable.lane == "vector" && unavailable.reason == EMBEDDINGS_OFF)
}

/// Hybrid lane: run the vector, keyword, and exact lanes best-effort and fuse their rankings via RRF
/// on the shared `chunk_id` key. Each lane is independent — a lane that is unavailable (no embedder,
/// read-only index) or that a non-identifier query doesn't trigger simply contributes nothing; the
/// query never fails on a single lane. The returned hits carry the fused RRF score in `score`.
fn hybrid_hits(
    ws: &Workspace,
    query: &str,
    limit: usize,
    cancelled: &dyn Fn() -> bool,
) -> Result<(Vec<CodeSearchHit>, LaneReport), EngineError> {
    // A scoped search ranks deeper than 200, so the cap must not fall below `limit`.
    let fuse_limit = limit.saturating_mul(4).min(200).max(limit);

    let mut report = LaneReport::default();

    // `vector_ran` is tracked separately from `vector_ids.is_empty()`: a lane that ran and matched
    // nothing is a real answer, a lane that failed is not, and collapsing the two is how a failed
    // vector lane used to be reported as a complete result. With `embed` off the lane is not
    // degraded — it was never asked to run. ~keep
    let mut vector_ran = false;
    let vector_ids: Vec<String> = if ws.config.code_search.embed {
        match semantic_hits(ws, query, fuse_limit, None) {
            Ok(hits) => {
                vector_ran = true;
                report.ran.push("vector");
                hits.into_iter().map(|h| h.chunk_id).collect()
            }
            Err(error) => {
                tracing::debug!(%error, "hybrid: vector lane unavailable — fusing keyword + exact only");
                report.degrade(&["vector"], format!("the vector lane failed ({error})"));
                Vec::new()
            }
        }
    } else {
        // Not a failure, but the caller asked for a hybrid search and is getting a lexical one: a
        // result that silently lacks the vector lane reads as "nothing similar exists".
        report.unavailable.push(LaneUnavailable {
            lane: "vector".to_string(),
            reason: EMBEDDINGS_OFF.to_string(),
        });
        Vec::new()
    };

    stop_if(cancelled)?;
    let lanes = fjall_lanes(ws, query, fuse_limit, true);
    match &lanes.degraded {
        Some(degraded) => report.degrade(&["keyword", "exact"], degraded.reason.clone()),
        None => report.ran.extend(["keyword", "exact"]),
    }

    // A degraded-lane notice attached to an EMPTY hit list is still a success response, and a
    // caller reads it as "no matches" — the very defect this forward removes. Partial results are
    // only honest when some lane actually ran. ~keep
    if let Some(degraded) = lanes.degraded.as_ref().filter(|_| !vector_ran) {
        let reason = report.reason.clone().unwrap_or_default();
        return Err(lane_refusal(
            degraded.cause,
            format!(
                "`code_search`: no lane could run — {reason}. Nothing was searched; this is not an empty result set."
            ),
        ));
    }
    let keyword_ids: Vec<String> = lanes.keyword.iter().map(|(id, _)| id.clone()).collect();
    let exact_ids = lanes.exact;

    let fused = rrf_fuse_detailed(
        &[
            FusionLane::new(LANE_EXACT, &exact_ids, WEIGHT_EXACT),
            FusionLane::new(LANE_VECTOR, &vector_ids, WEIGHT_VECTOR),
            FusionLane::new(LANE_KEYWORD, &keyword_ids, WEIGHT_KEYWORD),
        ],
        DEFAULT_RRF_K,
    );

    // Every fused id is kept, not `limit` of them: hydration drops ids no file holds any more and
    // fills the page from the ones after them.
    let mut hits = Vec::with_capacity(fused.len());
    for fh in fused {
        let mut hit = seed(fh.chunk_id);
        hit.score = Some(fh.score);
        hit.matched_lanes = fh.lane_ranks.iter().map(|(name, _)| name.to_string()).collect();
        for (name, rank) in &fh.lane_ranks {
            match *name {
                LANE_EXACT => hit.exact_rank = Some(*rank),
                LANE_VECTOR => hit.vector_rank = Some(*rank),
                LANE_KEYWORD => hit.keyword_rank = Some(*rank),
                _ => {}
            }
        }
        hits.push(hit);
    }
    Ok((hits, report))
}

/// Which lanes did not run, and why — reported on the response so a caller can tell a partial
/// result from a complete one. Empty means every lane the config asked for ran.
#[derive(Default)]
struct LaneReport {
    /// Lanes that produced a ranking (possibly empty) for this query.
    ran: Vec<&'static str>,
    /// Lanes that did not run, each with its own reason.
    unavailable: Vec<LaneUnavailable>,
    reason: Option<String>,
}

impl LaneReport {
    /// The report of a single-lane query, whose one lane either ran or failed the call.
    fn ran_only(lane: &'static str) -> Self {
        Self {
            ran: vec![lane],
            ..Self::default()
        }
    }

    /// Record `lanes` as degraded. Reasons accumulate, because more than one lane can fail for
    /// different causes in the same query and collapsing them would hide one.
    fn degrade(&mut self, lanes: &[&'static str], reason: String) {
        self.unavailable.extend(lanes.iter().map(|lane| LaneUnavailable {
            lane: (*lane).to_string(),
            reason: reason.clone(),
        }));
        self.reason = Some(match self.reason.take() {
            Some(existing) => format!("{existing}; {reason}"),
            None => reason,
        });
    }
}

/// The error for lanes that did not run, on a call that has no other lane to answer with: busy while
/// another process holds the index, unavailable until a rescan rebuilds it.
fn lane_refusal(cause: LaneCause, message: String) -> EngineError {
    match cause {
        LaneCause::Held => EngineError::Busy(message),
        LaneCause::RescanOwed => EngineError::Unavailable(message),
    }
}

/// Run the keyword (BM25) + exact (symbol-name) lanes over this session's index.
///
/// Both lanes read fjall, whose directory lock is exclusive, so a session that holds no index (a CLI
/// command or a second `serve` beside a writer, with no daemon) cannot run them: the outcome names
/// why and the fix ([`Degraded::unreachable`]). A daemon-hosted session borrows the pool writer's
/// index handle, so it runs them like a writer. ~keep
fn fjall_lanes(ws: &Workspace, query: &str, limit: usize, want_exact: bool) -> LaneOutcome {
    let rescan_owed = {
        let store = ws.store_read();
        if let Some(db) = store.read_index_view() {
            let keyword = bm25_search(&*db, query, limit)
                .into_iter()
                .map(|hit| (hit.chunk_id, hit.score))
                .collect();
            let exact = if want_exact {
                exact_lane_chunk_ids(&store, &*db, query, limit)
            } else {
                Vec::new()
            };
            return LaneOutcome {
                keyword,
                exact,
                degraded: None,
            };
        }
        store.rescan_owed()
    };
    Degraded::unreachable(rescan_owed).into()
}

/// What a semantic-only `code_search` can do while the vector store is unavailable.
const SEMANTIC_MEANWHILE: Option<&str> = Some(
    "Meanwhile `lane: \"keyword\"` searches without it, and `lane: \"hybrid\"` runs the keyword and exact \
     lanes and names the vector lane in `lanes_unavailable`.",
);

/// Semantic lane: embed the query and run vector KNN over the scope-filtered LanceDB `code_chunks`
/// table. Each hit carries an L2 `distance` (lower = closer) and no BM25 `score`. A vector store
/// that is unavailable is refused, ending with `meanwhile`.
fn semantic_hits(
    ws: &Workspace,
    query: &str,
    limit: usize,
    meanwhile: Option<&str>,
) -> Result<Vec<CodeSearchHit>, EngineError> {
    let embedding = ws.embed_query(query)?;
    let lance = ws
        .lance_store()
        .map_err(|unavailable| unavailable.refusal("code_search", meanwhile))?;
    let hits_raw = lance
        .search_code_chunks(&ws.scope, embedding, limit)
        .map_err(|e| EngineError::internal(format!("search_code_chunks: {e}")))?;

    // A row's own path and metadata are not used: the chunk and its live paths are resolved by
    // `hydrate_hits`, like every other lane's.
    Ok(hits_raw
        .into_iter()
        .map(|h| {
            let mut hit = seed(h.chunk_id);
            hit.distance = Some(h.distance);
            hit
        })
        .collect())
}

/// Keyword lane: native BM25 over the Fjall index, hydrating each ranked `chunk_id` into a pointer.
/// Each hit carries a BM25 `score` (higher = better) and no `distance`. Refused, never an empty vec,
/// when this session cannot read the index.
fn keyword_hits(ws: &Workspace, query: &str, limit: usize) -> Result<Vec<CodeSearchHit>, EngineError> {
    let lanes = fjall_lanes(ws, query, limit, false);
    // `lane: "keyword"` has no other lane to fall back on, so a degraded read is a hard error. There
    // is no honest partial here — an empty list would claim the BM25 index was searched. ~keep
    if let Some(Degraded { reason, cause }) = lanes.degraded {
        return Err(lane_refusal(
            cause,
            format!(
                "`code_search` lane \"keyword\": the BM25 index cannot be read here: {reason}. No search ran; this \
             is not an empty result set."
            ),
        ));
    }
    let mut hits = Vec::with_capacity(lanes.keyword.len());
    for (rank, (chunk_id, score)) in lanes.keyword.into_iter().enumerate() {
        let mut ch = seed(chunk_id);
        ch.score = Some(score);
        // Provenance has to be set here too, or a keyword-only hit comes back claiming no lane
        // matched it. ~keep
        ch.matched_lanes = vec![LANE_KEYWORD.to_string()];
        ch.keyword_rank = u32::try_from(rank + 1).ok();
        hits.push(ch);
    }
    Ok(hits)
}

/// A ranked chunk id with nothing resolved yet; [`hydrate_hits`] fills in the rest.
fn seed(chunk_id: String) -> CodeSearchHit {
    CodeSearchHit {
        path: String::new(),
        also_at: Vec::new(),
        chunk_id,
        symbol: String::new(),
        kind: String::new(),
        lang: String::new(),
        line_start: 0,
        line_end: 0,
        byte_start: 0,
        byte_end: 0,
        distance: None,
        score: None,
        rerank_score: None,
        matched_lanes: Vec::new(),
        keyword_rank: None,
        vector_rank: None,
        exact_rank: None,
    }
}

/// The first `limit` resolvable hits of a ranking, the bodies of the first `keep_text` of them for
/// the reranker, and how many ranked chunks were dropped.
struct Hydrated {
    hits: Vec<CodeSearchHit>,
    texts: Vec<String>,
    omitted: usize,
}

/// Resolve ranked seeds into hits, in rank order: the chunk's metadata comes from its sidecar
/// (decoded once per content hash), `path` and `also_at` from the view's file map now, keeping the
/// paths `filters` passes and, with a `scope`, only those in its files. A chunk no indexed file holds
/// is dropped and counted; one held only by files the filters or the scope leave out is dropped
/// silently; an id ranked twice (one vector row per copy of a file) keeps its first rank.
/// Stops with [`EngineError::Cancelled`] once `cancelled` says so, checked every
/// [`HYDRATE_CANCEL_EVERY`] seeds.
fn hydrate_hits(
    store: &Store,
    seeds: Vec<CodeSearchHit>,
    limit: usize,
    keep_text: usize,
    filters: &Filters<'_>,
    scope: Option<&ChangeScope>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Hydrated, EngineError> {
    let mut hydrator = ChunkHydrator::new(store);
    hydrator.resolve_paths(seeds.iter().map(|s| s.chunk_id.as_str()));
    let mut seen: AHashSet<String> = AHashSet::with_capacity(seeds.len());
    let mut out = Hydrated {
        hits: Vec::with_capacity(seeds.len().min(limit)),
        texts: Vec::new(),
        omitted: 0,
    };
    for (at, mut hit) in seeds.into_iter().enumerate() {
        if out.hits.len() >= limit {
            break;
        }
        if at % HYDRATE_CANCEL_EVERY == 0 {
            stop_if(cancelled)?;
        }
        if seen.contains(hit.chunk_id.as_str()) {
            continue;
        }
        let live = hydrator.paths(&hit.chunk_id);
        if live.is_empty() {
            out.omitted += 1;
            continue;
        }
        let mut kept = live.iter().filter(|path| {
            super::changed::admits(scope, &RelPath::from(path.as_str()))
                && (filters.is_empty()
                    || store
                        .lookup(path.as_bytes())
                        .is_some_and(|entry| filters.passes(path.as_bytes(), &entry.language)))
        });
        let Some(path) = kept.next() else {
            continue;
        };
        let (path, also_at): (String, Vec<String>) = (path.clone(), kept.cloned().collect());
        let Some(chunk) = hydrator.chunk(&hit.chunk_id) else {
            out.omitted += 1;
            continue;
        };
        hit.path = path;
        hit.also_at = also_at;
        hit.symbol = chunk.symbol.clone().unwrap_or_default();
        hit.kind = chunk.kind.clone().unwrap_or_default();
        hit.lang.clone_from(&chunk.lang);
        hit.line_start = chunk.line_start;
        hit.line_end = chunk.line_end;
        hit.byte_start = chunk.byte_start;
        hit.byte_end = chunk.byte_end;
        if out.hits.len() < keep_text {
            out.texts.push(chunk.text.clone());
        }
        seen.insert(hit.chunk_id.clone());
        out.hits.push(hit);
    }
    Ok(out)
}

/// Most fused hits the reranker scores; the rest keep their fused order after them.
const RERANK_CANDIDATES: usize = 50;

/// Reorder the first hits, one per body in `texts` (at most [`RERANK_CANDIDATES`]), by the
/// cross-encoder, the rest following in fused order. Nothing is dropped. A failure returns the hits
/// as they came, with the reason.
fn rerank_hits(
    ws: &Workspace,
    query: &str,
    mut hits: Vec<CodeSearchHit>,
    texts: Vec<String>,
    preset: &str,
) -> (Vec<CodeSearchHit>, Option<String>) {
    if hits.is_empty() || texts.is_empty() {
        return (hits, None);
    }
    if xberg::get_reranker_preset(preset).is_none() {
        return (hits, Some(format!("unknown reranker preset {preset:?}")));
    }
    let candidates = texts.len().min(hits.len());
    let config = xberg::core::config::RerankerConfig {
        model: xberg::core::config::RerankerModelType::Preset {
            name: preset.to_string(),
        },
        top_k: Some(candidates),
        acceleration: crate::embeddings::acceleration(ws.config.resources.onnx_provider),
        ..Default::default()
    };
    let scored = match crate::embeddings::rerank(query.to_string(), texts, &config, preset) {
        Ok(scored) => scored,
        Err(error) => return (hits, Some(format!("the reranker failed: {error}"))),
    };
    let mut head: Vec<Option<CodeSearchHit>> = hits.drain(..candidates).map(Some).collect();
    let mut reordered = Vec::with_capacity(candidates);
    for result in &scored {
        if let Some(mut hit) = head.get_mut(result.index).and_then(Option::take) {
            hit.rerank_score = Some(result.score);
            reordered.push(hit);
        }
    }
    reordered.extend(head.into_iter().flatten());
    reordered.append(&mut hits);
    (reordered, None)
}

#[cfg(test)]
#[path = "code_search_tests.rs"]
mod tests;
