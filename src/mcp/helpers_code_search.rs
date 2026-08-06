//! Helper bodies for the `code` domain's retrieval modes (`semantic`, `chunk`).
//!
//! Gated on `feature = "code-search"`. `run_search_code` dispatches by `lane`: `hybrid` (default)
//! fuses the vector, keyword, and exact-symbol lanes via RRF ([`hybrid_hits`]); `semantic`
//! ([`semantic_hits`]) is vector KNN over the LanceDB `code_chunks` table; `keyword`
//! ([`keyword_hits`]) is native BM25 over the Fjall index. An optional cross-encoder [`rerank_hits`]
//! pass reorders the result. `run_get_chunk` is the offline fetch half — it reads the file's
//! content-addressed `.chunk` sidecar and returns one chunk's body, no LanceDB round-trip.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::{elapsed_us, json_result};
use super::memory::{embed_query, lance_store};
use super::types_code::{CodeSearchHit, GetChunkParams, GetChunkResponse, SearchCodeParams, SearchCodeResponse};
use crate::search::bm25::bm25_search;
use crate::search::exact::exact_lane_chunk_ids;
use crate::search::rrf::{
    DEFAULT_RRF_K, FusionLane, LANE_EXACT, LANE_KEYWORD, LANE_VECTOR, WEIGHT_EXACT, WEIGHT_KEYWORD, WEIGHT_VECTOR,
    rrf_fuse_detailed,
};
use crate::store::Store;

/// Serialize a code-search response honoring the requested wire format. TOON is only available
/// when the `documents` feature (which links `serde_toon`) is also compiled in; a `toon` request
/// on a code-search-only build silently falls back to JSON.
fn format_code_response<T: serde::Serialize>(value: &T, want_toon: bool) -> Result<CallToolResult, McpError> {
    #[cfg(feature = "documents")]
    if want_toon {
        return super::helpers::format_response(value, crate::config::OutputFormat::Toon);
    }
    let _ = want_toon;
    json_result(value)
}

/// Resolve whether the caller wants TOON output: an explicit `format` param wins; otherwise fall
/// back to the `[documents.output] format` config knob.
fn wants_toon(state: &ServerState, format: Option<&str>) -> bool {
    match format.map(str::trim) {
        Some(f) if f.eq_ignore_ascii_case("toon") => true,
        Some(f) if f.eq_ignore_ascii_case("json") => false,
        _ => matches!(
            state.shared.config.documents.output.format,
            crate::config::OutputFormat::Toon
        ),
    }
}

pub(super) async fn run_search_code(state: &ServerState, params: SearchCodeParams) -> Result<CallToolResult, McpError> {
    let __body = std::time::Instant::now();
    let limit = params.limit.unwrap_or(10).min(100) as usize;
    let want_toon = wants_toon(state, params.format.as_deref());

    let rr = &state.shared.config.code_search.reranker;
    let rerank_enabled = params.reranker_enabled.unwrap_or(rr.enabled);
    let rerank_preset = params.reranker_preset.clone().unwrap_or_else(|| rr.preset.clone());
    let rerank_top_k = params.reranker_top_k.unwrap_or(rr.top_k);

    let fetch_n = if rerank_enabled { limit.max(rerank_top_k) } else { limit };

    let mode = params.mode.as_deref().map(str::trim).unwrap_or("hybrid");
    let (hits, report): (Vec<CodeSearchHit>, LaneReport) = if mode.is_empty() || mode.eq_ignore_ascii_case("hybrid") {
        hybrid_hits(state, &params.query, fetch_n).await?
    } else if mode.eq_ignore_ascii_case("semantic") {
        (
            semantic_hits(state, &params.query, fetch_n).await?,
            LaneReport::default(),
        )
    } else if mode.eq_ignore_ascii_case("keyword") {
        (
            keyword_hits(state, &params.query, fetch_n).await?,
            LaneReport::default(),
        )
    } else {
        return Err(McpError::invalid_request(
            format!(
                "`code` mode=\"semantic\": unknown lane {mode:?}; expected \"hybrid\", \"semantic\", or \"keyword\""
            ),
            None,
        ));
    };

    let hits = if rerank_enabled {
        rerank_hits(state, &params.query, hits, &rerank_preset, rerank_top_k).await?
    } else {
        hits
    };

    let budget = super::budget::apply_budget(hits, params.max_tokens);
    format_code_response(
        &SearchCodeResponse {
            query: params.query,
            budgeted: budget.budgeted,
            hits: budget.items,
            degraded_lanes: report.lanes,
            degraded_reason: report.reason,
            elapsed_us: elapsed_us(__body),
        },
        want_toon,
    )
}

/// Hybrid lane: run the vector, keyword, and exact lanes best-effort and fuse their rankings via RRF
/// on the shared `chunk_id` key. Each lane is independent — a lane that is unavailable (no embedder,
/// read-only index) or that a non-identifier query doesn't trigger simply contributes nothing; the
/// query never fails on a single lane. The returned hits carry the fused RRF score in `score`.
async fn hybrid_hits(
    state: &ServerState,
    query: &str,
    limit: usize,
) -> Result<(Vec<CodeSearchHit>, LaneReport), McpError> {
    let fuse_limit = (limit * 4).clamp(limit, 200);

    let mut report = LaneReport::default();

    // `vector_ran` is tracked separately from `vector_ids.is_empty()`: a lane that ran and matched
    // nothing is a real answer, a lane that failed is not, and collapsing the two is how a failed
    // vector lane used to be reported as a complete result. With `embed` off the lane is not
    // degraded — it was never asked to run. ~keep
    let mut vector_ran = false;
    let vector_ids: Vec<String> = if state.shared.config.code_search.embed {
        match semantic_hits(state, query, fuse_limit).await {
            Ok(hits) => {
                vector_ran = true;
                hits.into_iter().map(|h| h.chunk_id).collect()
            }
            Err(error) => {
                tracing::debug!(%error, "hybrid: vector lane unavailable — fusing keyword + exact only");
                report.degrade(&["vector"], format!("the vector lane failed ({error})"));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let lanes = fjall_lanes(state, query, fuse_limit, true).await?;
    if let Some(reason) = &lanes.degraded {
        report.degrade(&["keyword", "exact"], reason.clone());
    }

    // A degraded-lane notice attached to an EMPTY hit list is still a success response, and a
    // caller reads it as "no matches" — the very defect this forward removes. Partial results are
    // only honest when some lane actually ran. ~keep
    if lanes.degraded.is_some() && !vector_ran {
        let reason = report.reason.clone().unwrap_or_default();
        return Err(McpError::internal_error(
            format!(
                "`code` mode=\"semantic\": no lane could run — {reason}. Nothing was searched; this \
                 is not an empty result set."
            ),
            None,
        ));
    }
    let keyword_ids: Vec<String> = lanes.keyword.iter().map(|(id, _)| id.clone()).collect();
    let exact_ids = lanes.exact;

    let store = state.shared.store.read().await;
    let fused = rrf_fuse_detailed(
        &[
            FusionLane::new(LANE_EXACT, &exact_ids, WEIGHT_EXACT),
            FusionLane::new(LANE_VECTOR, &vector_ids, WEIGHT_VECTOR),
            FusionLane::new(LANE_KEYWORD, &keyword_ids, WEIGHT_KEYWORD),
        ],
        DEFAULT_RRF_K,
    );

    let mut hits = Vec::with_capacity(fused.len().min(limit));
    for fh in fused.into_iter().take(limit) {
        if let Some((mut hit, _text)) = hydrate_one(&store, &fh.chunk_id) {
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
    }
    Ok((hits, report))
}

/// Which lanes did not run, and why — reported on the response so a caller can tell a partial
/// result from a complete one. Empty means every lane the config asked for ran.
#[derive(Default)]
struct LaneReport {
    lanes: Vec<String>,
    reason: Option<String>,
}

impl LaneReport {
    /// Record `lanes` as degraded. Reasons accumulate, because more than one lane can fail for
    /// different causes in the same query and collapsing them would hide one.
    fn degrade(&mut self, lanes: &[&str], reason: String) {
        self.lanes.extend(lanes.iter().map(|l| (*l).to_string()));
        self.reason = Some(match self.reason.take() {
            Some(existing) => format!("{existing}; {reason}"),
            None => reason,
        });
    }
}

/// The two fjall-backed lanes' rankings, plus why they could not run.
struct LaneOutcome {
    /// BM25 `(chunk_id, score)`, best first.
    keyword: Vec<(String, f32)>,
    /// Exact symbol-name lane chunk ids, best first.
    exact: Vec<String>,
    /// `None` when the lanes ran. `Some(reason)` when they could not — never conflated with "ran and
    /// matched nothing", which is what the pre-forward code returned.
    degraded: Option<String>,
}

/// Run the keyword (BM25) + exact (symbol-name) lanes, forwarding to the daemon when this session
/// holds no `IndexDb`.
///
/// Both lanes read fjall, whose directory lock is exclusive, so only the daemon can serve them on a
/// machine where it is running. A reader session forwards; a HOSTED session (running inside the
/// daemon) must never forward, because that is the daemon dialling itself — the re-entrancy the
/// hosted comms path had to remove. A hosted stack reaches the pool directly instead. ~keep
async fn fjall_lanes(
    state: &ServerState,
    query: &str,
    limit: usize,
    want_exact: bool,
) -> Result<LaneOutcome, McpError> {
    {
        let store = state.shared.store.read().await;
        if let Some(db) = store.index_db.as_ref() {
            let keyword = bm25_search(db, query, limit)
                .into_iter()
                .map(|hit| (hit.chunk_id, hit.score))
                .collect();
            let exact = if want_exact {
                exact_lane_chunk_ids(&store, db, query, limit)
            } else {
                Vec::new()
            };
            return Ok(LaneOutcome {
                keyword,
                exact,
                degraded: None,
            });
        }
    }
    forward_fjall_lanes(state, query, limit, want_exact).await
}

/// Ask the daemon — the sole fjall writer — for the two lanes' rankings. Ranking only: chunk bodies
/// come from content-addressed blobs this session can already read, so nothing else crosses the wire.
#[cfg(all(feature = "comms", any(unix, windows)))]
async fn forward_fjall_lanes(
    state: &ServerState,
    query: &str,
    limit: usize,
    want_exact: bool,
) -> Result<LaneOutcome, McpError> {
    use crate::comms::code_search_proto::CodeSearchLaneQuery;

    let request = CodeSearchLaneQuery {
        query: query.to_string(),
        limit: limit as u32,
        want_exact,
    };
    let root = state.shared.root.clone();

    // A DAEMON-HOSTED stack reaches the pool's read-write index directly through the host seam. It
    // must never take the socket path below: that is the daemon dialling itself. Same branch order
    // as the resolved-refs site, for the same reason. ~keep
    if let Some(host) = state.shared.host.as_ref() {
        let host = std::sync::Arc::clone(host);
        let hosted = tokio::task::spawn_blocking(move || host.host_code_search_lanes(&root, request))
            .await
            .map_err(|join| McpError::internal_error(format!("code_search_lanes host join: {join}"), None))?;
        return match hosted {
            Ok(result) => Ok(LaneOutcome {
                keyword: result.keyword,
                exact: result.exact,
                degraded: None,
            }),
            Err(error) => Ok(LaneOutcome {
                keyword: Vec::new(),
                exact: Vec::new(),
                degraded: Some(format!("the hosted index read failed ({error})")),
            }),
        };
    }

    let mut client = match super::helpers_comms::connect_ephemeral_client(state).await {
        Ok(client) => client,
        Err(error) => {
            return Ok(LaneOutcome {
                keyword: Vec::new(),
                exact: Vec::new(),
                degraded: Some(format!("the daemon is unreachable ({error})")),
            });
        }
    };
    match client.code_search_lanes(root, request).await {
        Ok(result) => Ok(LaneOutcome {
            keyword: result.keyword,
            exact: result.exact,
            degraded: None,
        }),
        Err(error) => Ok(LaneOutcome {
            keyword: Vec::new(),
            exact: Vec::new(),
            degraded: Some(format!("the daemon refused the lane read ({error})")),
        }),
    }
}

/// Without `comms` there is no daemon to ask, so a reader session simply has no keyword lane. Still
/// reported as degraded rather than empty — the caller turns that into an error or a labelled
/// partial, never a bare `[]`.
#[cfg(not(all(feature = "comms", any(unix, windows))))]
async fn forward_fjall_lanes(
    _state: &ServerState,
    _query: &str,
    _limit: usize,
    _want_exact: bool,
) -> Result<LaneOutcome, McpError> {
    Ok(LaneOutcome {
        keyword: Vec::new(),
        exact: Vec::new(),
        degraded: Some("this build has no `comms` feature, so there is no daemon to read it".to_string()),
    })
}

/// Semantic lane: embed the query and run vector KNN over the scope-filtered LanceDB `code_chunks`
/// table. Each hit carries an L2 `distance` (lower = closer) and no BM25 `score`.
async fn semantic_hits(state: &ServerState, query: &str, limit: usize) -> Result<Vec<CodeSearchHit>, McpError> {
    let embedding = embed_query(state, query).await?;
    let lance = lance_store(state).await?;
    let scope = state.shared.scope.clone();
    let hits_raw = tokio::task::spawn_blocking(move || lance.search_code_chunks(&scope, embedding, limit))
        .await
        .map_err(|e| McpError::internal_error(format!("spawn_blocking: {e}"), None))?
        .map_err(|e| McpError::internal_error(format!("search_code_chunks: {e}"), None))?;

    Ok(hits_raw
        .into_iter()
        .map(|h| CodeSearchHit {
            path: h.path,
            chunk_id: h.chunk_id,
            symbol: h.symbol,
            kind: h.kind,
            lang: h.lang,
            line_start: h.line_start,
            line_end: h.line_end,
            byte_start: h.byte_start,
            byte_end: h.byte_end,
            distance: Some(h.distance),
            score: None,
            rerank_score: None,
            matched_lanes: Vec::new(),
            keyword_rank: None,
            vector_rank: None,
            exact_rank: None,
        })
        .collect())
}

/// Keyword lane: native BM25 over the Fjall index, hydrating each ranked `chunk_id` into a pointer.
/// Each hit carries a BM25 `score` (higher = better) and no `distance`. Returns an empty vec when the
/// index is read-only (no `IndexDb` handle) — there is no keyword lane on a reader session.
async fn keyword_hits(state: &ServerState, query: &str, limit: usize) -> Result<Vec<CodeSearchHit>, McpError> {
    let lanes = fjall_lanes(state, query, limit, false).await?;
    // `lane: "keyword"` has no other lane to fall back on, so a degraded read is a hard error. There
    // is no honest partial here — an empty list would claim the BM25 index was searched. ~keep
    if let Some(reason) = lanes.degraded {
        return Err(McpError::internal_error(
            format!(
                "`code` mode=\"semantic\" lane=\"keyword\": the BM25 index lives in the daemon's \
                 fjall store and {reason}. No search ran; this is not an empty result set."
            ),
            None,
        ));
    }
    let store = state.shared.store.read().await;
    let mut hits = Vec::with_capacity(lanes.keyword.len());
    for (rank, (chunk_id, score)) in lanes.keyword.into_iter().enumerate() {
        if let Some((mut ch, _text)) = hydrate_one(&store, &chunk_id) {
            ch.score = Some(score);
            // Provenance has to be set here too. `hydrate_one` leaves it empty and only the hybrid
            // path filled it in from the RRF lane ranks, so a keyword-only hit came back claiming no
            // lane matched it — the same "the response does not say what actually happened" problem
            // as the silent empty, one level down. ~keep
            ch.matched_lanes = vec![LANE_KEYWORD.to_string()];
            ch.keyword_rank = u32::try_from(rank + 1).ok();
            hits.push(ch);
        }
    }
    Ok(hits)
}

/// Hydrate a ranked `chunk_id` (`<hash>:<ordinal>`) into a base `CodeSearchHit` (all score fields
/// `None`) plus the chunk's body text (for the optional rerank pass), via the content-addressed
/// sidecar. `None` when the sidecar is missing or the ordinal is out of range — the caller skips it.
fn hydrate_one(store: &Store, chunk_id: &str) -> Option<(CodeSearchHit, String)> {
    let (hash_hex, ordinal) = chunk_id.rsplit_once(':')?;
    let ordinal: usize = ordinal.parse().ok()?;
    let blob = store.read_chunks_by_hex(hash_hex).ok()??;
    let chunk = blob.chunks.get(ordinal)?;
    let hit = CodeSearchHit {
        path: chunk.path.clone(),
        chunk_id: chunk_id.to_string(),
        symbol: chunk.symbol.clone().unwrap_or_default(),
        kind: chunk.kind.clone().unwrap_or_default(),
        lang: chunk.lang.clone(),
        line_start: chunk.line_start,
        line_end: chunk.line_end,
        byte_start: chunk.byte_start,
        byte_end: chunk.byte_end,
        distance: None,
        score: None,
        rerank_score: None,
        matched_lanes: Vec::new(),
        keyword_rank: None,
        vector_rank: None,
        exact_rank: None,
    };
    Some((hit, chunk.text.clone()))
}

/// Optional cross-encoder rerank of `hits`, reusing the same xberg reranker as the documents tier.
/// Reads each hit's chunk body as the candidate text, scores against `query`, and returns the hits
/// reordered best-first (truncated to `top_k`) with `rerank_score` set. Off-path when `hits` is
/// empty. Errors on an unknown preset (before any model download) or an out-of-range rerank index.
async fn rerank_hits(
    state: &ServerState,
    query: &str,
    hits: Vec<CodeSearchHit>,
    preset: &str,
    top_k: usize,
) -> Result<Vec<CodeSearchHit>, McpError> {
    if hits.is_empty() {
        return Ok(hits);
    }
    if xberg::get_reranker_preset(preset).is_none() {
        return Err(McpError::invalid_params(
            format!("unknown reranker preset: {preset:?}"),
            None,
        ));
    }
    let texts: Vec<String> = {
        let store = state.shared.store.read().await;
        hits.iter()
            .map(|h| {
                hydrate_one(&store, &h.chunk_id)
                    .map(|(_, text)| text)
                    .unwrap_or_default()
            })
            .collect()
    };
    let krz_config = xberg::core::config::RerankerConfig {
        model: xberg::core::config::RerankerModelType::Preset {
            name: preset.to_string(),
        },
        top_k: Some(top_k),
        ..Default::default()
    };
    let reranked = xberg::rerank_async(query.to_string(), texts, &krz_config)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            let kind = if msg.contains("download") || msg.contains("HuggingFace") || msg.contains("model") {
                "rerank model load"
            } else {
                "rerank inference"
            };
            McpError::internal_error(format!("{kind}: {msg}"), None)
        })?;
    let original = hits;
    reranked
        .into_iter()
        .map(|r| {
            original
                .get(r.index)
                .cloned()
                .map(|mut hit| {
                    hit.rerank_score = Some(r.score);
                    hit
                })
                .ok_or_else(|| {
                    McpError::internal_error(
                        format!(
                            "reranker returned out-of-range index {} (got {} hits)",
                            r.index,
                            original.len()
                        ),
                        None,
                    )
                })
        })
        .collect()
}

pub(super) async fn run_get_chunk(state: &ServerState, params: GetChunkParams) -> Result<CallToolResult, McpError> {
    let __body = std::time::Instant::now();
    let blob = {
        let store = state.shared.store.read().await;
        let entry = store.lookup(&params.path).ok_or_else(|| {
            McpError::invalid_params(format!("`code` mode `chunk`: file not indexed: {}", params.path), None)
        })?;
        let hash_hex = entry.hash_hex.clone();
        store
            .read_chunks_by_hex(&hash_hex)
            .map_err(|e| McpError::internal_error(format!("`code` mode `chunk`: read chunk blob: {e}"), None))?
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!(
                        "`code` mode `chunk`: no code chunks indexed for {} (scan with --features code-search)",
                        params.path
                    ),
                    None,
                )
            })?
    };

    let chunks = &blob.chunks;
    if chunks.is_empty() {
        return Err(McpError::invalid_params(
            format!("`code` mode `chunk`: {} has no chunks", params.path),
            None,
        ));
    }

    let chunk = if let Some(id) = params.chunk_id.as_deref() {
        chunks.iter().find(|c| c.chunk_id == id).ok_or_else(|| {
            McpError::invalid_params(
                format!("`code` mode `chunk`: chunk_id {id:?} not found in {}", params.path),
                None,
            )
        })?
    } else if let Some(bs) = params.byte_start {
        chunks.iter().find(|c| c.byte_start == bs).ok_or_else(|| {
            McpError::invalid_params(
                format!("`code` mode `chunk`: no chunk at byte_start {bs} in {}", params.path),
                None,
            )
        })?
    } else if chunks.len() == 1 {
        &chunks[0]
    } else {
        let ids: Vec<&str> = chunks.iter().map(|c| c.chunk_id.as_str()).collect();
        return Err(McpError::invalid_params(
            format!(
                "`code` mode `chunk`: {} has {} chunks; pass `chunk_id` or `byte_start` to disambiguate: {}",
                params.path,
                chunks.len(),
                ids.join(", ")
            ),
            None,
        ));
    };

    json_result(&GetChunkResponse {
        path: chunk.path.clone(),
        chunk_id: chunk.chunk_id.clone(),
        symbol: chunk.symbol.clone(),
        kind: chunk.kind.clone(),
        lang: chunk.lang.clone(),
        signature: chunk.signature.clone(),
        doc: chunk.doc.clone(),
        line_start: chunk.line_start,
        line_end: chunk.line_end,
        byte_start: chunk.byte_start,
        byte_end: chunk.byte_end,
        text: chunk.text.clone(),
        elapsed_us: elapsed_us(__body),
    })
}
