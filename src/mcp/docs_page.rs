//! A page of a `docs_search` ranking (ADR-0029 rules 7 and 8): rerank the head of the ranking,
//! then cut the page, merge adjacent chunks of one document, add a neighbouring chunk on either
//! side while the token budget allows, and cite every hit.

use ahash::AHashMap;
use rmcp::ErrorData as McpError;

use super::ServerState;
use super::cursor::Cursor;
use super::types_documents::{
    DocChunkRef, DocCite, DocContext, DocHit, DocsRanking, DocsSearchResponse, RankedChunk, RerankReport,
};
use crate::lance::docs_schema::DocChunkHit;

/// Fused hits the reranker scores; the rest follow in fused order (ADR-0029 rule 7).
pub(super) const RERANK_CANDIDATES: usize = 50;
/// The longest overlap between consecutive chunks that stitching looks for, in bytes.
const STITCH_MAX_OVERLAP: usize = 512;
/// The shortest shared text taken for an overlap: a shorter match is a coincidence ("the" + "end").
const STITCH_MIN_OVERLAP: usize = 12;

/// Reorder the first [`RERANK_CANDIDATES`] hits by the cross-encoder when the reranker is on.
/// Nothing is dropped, and a failure leaves the fused order with the reason on the report.
pub(super) async fn rerank(state: &ServerState, query: &str, ranking: &mut DocsRanking) {
    let config = &state.shared.config.documents.reranker;
    if !config.enabled || ranking.hits.is_empty() {
        return;
    }
    let candidates = ranking.hits.len().min(RERANK_CANDIDATES).min(config.top_k.max(1));
    let fail = |reason: String| RerankReport {
        ran: false,
        candidates: None,
        reason: Some(reason),
    };
    if xberg::get_reranker_preset(&config.preset).is_none() {
        ranking.rerank = fail(format!("unknown reranker preset {:?}", config.preset));
        return;
    }
    let documents: Vec<String> = ranking.hits[..candidates]
        .iter()
        .map(|hit| match &hit.cite.section {
            Some(section) if !section.is_empty() => format!("{}\n{}", section.join(" > "), hit.text),
            _ => hit.text.clone(),
        })
        .collect();
    let settings = xberg::core::config::RerankerConfig {
        model: xberg::core::config::RerankerModelType::Preset {
            name: config.preset.clone(),
        },
        top_k: Some(candidates),
        acceleration: crate::embeddings::acceleration(state.shared.config.resources.onnx_provider),
        ..Default::default()
    };
    match xberg::rerank_async(query.to_string(), documents, &settings).await {
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

/// Put the first `candidates` hits in the order of `scored` (index into them, score), each scored
/// hit carrying its score. A hit the reranker left out keeps its fused place after the scored ones.
fn reorder(hits: &mut Vec<RankedChunk>, candidates: usize, scored: &[xberg::RerankedDocument]) -> Result<(), String> {
    let mut head: Vec<Option<RankedChunk>> = hits.drain(..candidates).map(Some).collect();
    let mut reordered = Vec::with_capacity(candidates);
    for result in scored {
        let Some(slot) = head.get_mut(result.index) else {
            hits.splice(0..0, head.into_iter().flatten().chain(reordered));
            return Err(format!(
                "the reranker returned index {} of {candidates} candidates",
                result.index
            ));
        };
        if let Some(mut hit) = slot.take() {
            hit.rerank_score = Some(result.score);
            reordered.push(hit);
        }
    }
    reordered.extend(head.into_iter().flatten());
    hits.splice(0..0, reordered);
    Ok(())
}

/// One hit of a page before its context is added: a run of adjacent chunks of one document.
struct Merged {
    first: RankedChunk,
    members: Vec<RankedChunk>,
}

impl Merged {
    fn ord_range(&self) -> (u32, u32) {
        let ords = self.members.iter().map(|m| m.ord);
        (ords.clone().min().unwrap_or(0), ords.max().unwrap_or(0))
    }
}

/// Group the page's hits: a hit whose chunk is next to a chunk of the hit ranked just before it, in
/// the same document, joins that hit. Only rank neighbours merge, so every group is a run of
/// consecutive ranks and the page can end after any group. The group keeps its first rank.
fn merge(page: Vec<RankedChunk>) -> Vec<Merged> {
    let mut groups: Vec<Merged> = Vec::new();
    for hit in page {
        let joined = groups.last_mut().filter(|group| {
            group.first.doc == hit.doc && {
                let (lo, hi) = group.ord_range();
                hit.ord + 1 >= lo && hit.ord <= hi + 1
            }
        });
        match joined {
            Some(group) => group.members.push(hit),
            None => groups.push(Merged {
                first: hit.clone(),
                members: vec![hit],
            }),
        }
    }
    for group in &mut groups {
        group.members.sort_by_key(|member| member.ord);
    }
    groups
}

/// `b` appended to `a`, without the text the two chunks share at the seam (xberg overlaps
/// consecutive chunks).
fn stitch(a: &str, b: &str) -> String {
    let max = a.len().min(b.len()).min(STITCH_MAX_OVERLAP);
    let overlap = (STITCH_MIN_OVERLAP..=max)
        .rev()
        .find(|&n| a.is_char_boundary(a.len() - n) && b.is_char_boundary(n) && a[a.len() - n..] == b[..n])
        .unwrap_or(0);
    let rest = &b[overlap..];
    if overlap == 0 && !a.ends_with('\n') && !rest.starts_with('\n') {
        format!("{a}\n{rest}")
    } else {
        format!("{a}{rest}")
    }
}

fn union(cites: impl Iterator<Item = DocCite>) -> DocCite {
    let mut out: Option<DocCite> = None;
    for cite in cites {
        out = Some(match out {
            None => cite,
            Some(mut acc) => {
                acc.page_start = min_opt(acc.page_start, cite.page_start);
                acc.page_end = acc.page_end.max(cite.page_end);
                acc.line_start = min_opt(acc.line_start, cite.line_start);
                acc.line_end = acc.line_end.max(cite.line_end);
                acc.byte_start = acc.byte_start.min(cite.byte_start);
                acc.byte_end = acc.byte_end.max(cite.byte_end);
                acc
            }
        });
    }
    out.unwrap_or_default()
}

fn min_opt(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn tokens_of<T: serde::Serialize>(value: &T) -> u64 {
    serde_json::to_vec(value).map_or(0, |bytes| super::savings::bytes_to_tokens(bytes.len() as u64))
}

fn hit_of(group: &Merged, title: Option<String>, of: Option<u32>) -> DocHit {
    let (ord, ord_end) = group.ord_range();
    let text = group
        .members
        .iter()
        .skip(1)
        .fold(group.members[0].text.clone(), |acc, member| stitch(&acc, &member.text));
    let mut cite = union(group.members.iter().map(|member| member.cite.clone()));
    cite.section = group.members[0].cite.section.clone();
    DocHit {
        path: group.first.paths.first().cloned().unwrap_or_default(),
        also_at: group.first.paths.iter().skip(1).cloned().collect(),
        mime_type: group.first.mime_type.clone(),
        title,
        chunk: DocChunkRef {
            doc: group.first.doc.clone(),
            ord,
            ord_end,
            of,
        },
        cite,
        text,
        context: DocContext::default(),
        score: group.first.score,
        lanes: group.first.lanes,
        rerank_score: group.first.rerank_score,
    }
}

/// Cut the page `[offset, offset + limit)` out of `ranking` and build its hits.
pub(super) async fn page(
    state: &ServerState,
    ranking: DocsRanking,
    offset: u32,
    limit: u32,
    max_tokens: Option<u32>,
    generation: u32,
) -> Result<DocsSearchResponse, McpError> {
    use super::helpers_page::RANKED_DEPTH;

    let DocsRanking {
        query,
        mut hits,
        lanes_ran,
        lanes_unavailable,
        lexical_fuzzy,
        vector_mode,
        rerank,
        notice,
        truncated_reason,
    } = ranking;
    let goes_on = hits.len() > RANKED_DEPTH as usize;
    hits.truncate(RANKED_DEPTH as usize);
    let fetched = hits.len() as u32;
    let page: Vec<RankedChunk> = hits.into_iter().skip(offset as usize).take(limit as usize).collect();
    let consumed = page.len() as u32;
    let groups = merge(page);

    let store = if groups.is_empty() {
        None
    } else {
        super::docs_pipeline::docs_store(state).await?
    };
    let (metas, neighbours) = match &store {
        Some(store) => context_of(store, &groups).await,
        None => (AHashMap::new(), AHashMap::new()),
    };

    let mut out: Vec<DocHit> = groups
        .iter()
        .map(|group| {
            let meta = metas.get(&group.first.doc);
            hit_of(group, meta.and_then(|m| m.0.clone()), meta.map(|m| m.1))
        })
        .collect();
    let budget = max_tokens.map(u64::from);
    let mut used: u64 = 0;
    let mut kept = 0;
    for hit in &out {
        let cost = tokens_of(hit);
        if kept > 0 && budget.is_some_and(|budget| used + cost > budget) {
            break;
        }
        used += cost;
        kept += 1;
    }
    let budgeted = kept < out.len();
    out.truncate(kept);
    // Context goes on in rank order while the budget holds: before, then after.
    for (hit, group) in out.iter_mut().zip(&groups) {
        let (ord, ord_end) = group.ord_range();
        let before = ord
            .checked_sub(1)
            .and_then(|o| neighbours.get(&(group.first.doc.clone(), o)));
        let after = neighbours.get(&(group.first.doc.clone(), ord_end + 1));
        if let Some(chunk) = before {
            let cost = super::savings::bytes_to_tokens(chunk.text.len() as u64);
            if budget.is_none_or(|budget| used + cost <= budget) {
                used += cost;
                hit.text = stitch(&chunk.text, &hit.text);
                hit.context.before = 1;
            }
        }
        if let Some(chunk) = after {
            let cost = super::savings::bytes_to_tokens(chunk.text.len() as u64);
            if budget.is_none_or(|budget| used + cost <= budget) {
                used += cost;
                hit.text = stitch(&hit.text, &chunk.text);
                hit.context.after = 1;
            }
        }
    }

    // The page ends where its last kept hit's members end in the ranking.
    let kept_members: u32 = groups.iter().take(kept).map(|g| g.members.len() as u32).sum();
    let next = offset + if budgeted { kept_members } else { consumed };
    let more = !out.is_empty() && next < fetched;
    let truncated_reason = truncated_reason.or_else(|| goes_on.then(|| "depth".to_string()));
    Ok(DocsSearchResponse {
        query,
        hits: out,
        lanes_ran,
        lanes_unavailable,
        lexical_fuzzy,
        vector_mode,
        rerank,
        notice,
        truncated: truncated_reason.is_some(),
        truncated_reason,
        budgeted,
        next_cursor: more.then(|| Cursor::encode_in_memory(u64::from(next), generation)),
        elapsed_us: 0,
    })
}

type Metas = AHashMap<String, (Option<String>, u32)>;
type Neighbours = AHashMap<(String, u32), DocChunkHit>;

/// Titles and chunk counts of the page's documents, and the chunks on either side of each group.
/// A store error leaves the page without context rather than failing it.
async fn context_of(store: &crate::lance::docs::DocStore, groups: &[Merged]) -> (Metas, Neighbours) {
    let mut keys: Vec<String> = groups.iter().map(|g| g.first.doc.clone()).collect();
    keys.sort_unstable();
    keys.dedup();
    let mut wanted: AHashMap<String, Vec<u32>> = AHashMap::new();
    for group in groups {
        let (ord, ord_end) = group.ord_range();
        let ords = wanted.entry(group.first.doc.clone()).or_default();
        if let Some(before) = ord.checked_sub(1) {
            ords.push(before);
        }
        ords.push(ord_end + 1);
    }
    let wanted: Vec<(String, Vec<u32>)> = wanted.into_iter().collect();
    let store = store.clone();
    let fetched = tokio::task::spawn_blocking(move || {
        let metas = store.documents(&keys)?;
        let chunks = store.chunks(&wanted)?;
        anyhow::Ok((metas, chunks))
    })
    .await;
    match fetched {
        Ok(Ok((metas, chunks))) => (
            metas
                .into_iter()
                .map(|meta| (meta.doc_key, (meta.title, meta.n_chunks)))
                .collect(),
            chunks
                .into_iter()
                .map(|chunk| ((chunk.doc_key.clone(), chunk.chunk_ord), chunk))
                .collect(),
        ),
        Ok(Err(error)) => {
            tracing::warn!(%error, "docs_search: reading context chunks failed; the page carries none");
            (AHashMap::new(), AHashMap::new())
        }
        Err(error) => {
            tracing::warn!(%error, "docs_search: context lookup did not finish");
            (AHashMap::new(), AHashMap::new())
        }
    }
}

#[cfg(test)]
#[path = "docs_page_tests.rs"]
mod tests;
