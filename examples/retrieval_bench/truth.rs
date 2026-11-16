//! Ground truth and scoring. The truth is a brute-force pass over the corpus files with the filter
//! applied before ranking; it shares no code path with any backend.

use std::collections::BTreeMap;
use std::path::Path;

use ahash::{AHashMap, AHashSet};
use anyhow::{Context, Result, ensure};
use basemind::eval::{Item, score};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::backend::{Backend, Filter, Hit, VectorMode};
use crate::corpus::{CHUNKS_PER_DOC, Columns, Params, Query, doc_key, is_live, open_ipc};
use crate::fjall_flat::{bm25, group_code, tokens};

pub const TRUTH_K: usize = 10;
pub const SELECTIVITIES: [u32; 3] = [100, 10, 1];
/// Share of documents in the worktree's live-key map; 100 means no liveness predicate.
pub const LIVE_PERCENTS: [u64; 3] = [100, 90, 50];
/// Liveness widening of design section 2.2: start at `max(3K, K + 64)`, double, stop at this cap.
const LIVE_FETCH_CAP: usize = 4096;
/// Longest live-key list applied as a predicate (ADR-0029 rule 3).
pub const MAX_PREDICATE_KEYS: usize = 20_000;

#[derive(Debug, Serialize, Deserialize)]
pub struct Truth {
    pub state: String,
    pub rows: u64,
    /// `"<lane>/sel<percent>/live<percent>"` → per query, the top `TRUTH_K` ids, best first.
    pub entries: BTreeMap<String, Vec<Vec<String>>>,
}

pub fn variant(lane: &str, selectivity: u32, live: u64) -> String {
    format!("{lane}/sel{selectivity}/live{live}")
}

impl Truth {
    pub fn load(dir: &Path, state: &str) -> Result<Self> {
        let path = dir.join(format!("truth.{state}.json"));
        let body = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        Ok(serde_json::from_slice(&body)?)
    }
}

pub fn load_queries(dir: &Path) -> Result<Vec<Query>> {
    let path = dir.join("queries.json");
    let body = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(serde_json::from_slice(&body)?)
}

/// Eight independent accumulators so the sum vectorises; a single f32 accumulator cannot.
fn l2(a: &[f32], b: &[f32]) -> f32 {
    let mut lanes = [0.0f32; 8];
    let ((a_blocks, a_tail), (b_blocks, b_tail)) = (a.as_chunks::<8>(), b.as_chunks::<8>());
    let tail: f32 = a_tail.iter().zip(b_tail).map(|(x, y)| (x - y) * (x - y)).sum();
    for (x, y) in a_blocks.iter().zip(b_blocks) {
        for i in 0..8 {
            let d = x[i] - y[i];
            lanes[i] += d * d;
        }
    }
    lanes.iter().sum::<f32>() + tail
}

/// A row's token count and, per query term it holds, `(term, term frequency, token count)`.
type RowTerms = (u64, Vec<(String, f32, f32)>);

fn keep_best<T: Copy>(best: &mut Vec<(f32, T)>, candidate: (f32, T), better: impl Fn(f32, f32) -> bool) {
    if best.len() < TRUTH_K || better(candidate.0, best[best.len() - 1].0) {
        let at = best.partition_point(|kept| !better(candidate.0, kept.0));
        best.insert(at, candidate);
        best.truncate(TRUTH_K);
    }
}

fn render(seed: u64, row: u64) -> String {
    format!("{}:{}", doc_key(seed, row / CHUNKS_PER_DOC), row % CHUNKS_PER_DOC)
}

/// Brute-force truth for `state` (`base`, or `appended` to include `append.arrow`).
pub fn compute(dir: &Path, state: &str) -> Result<Truth> {
    let params = Params::load(dir)?;
    let queries = load_queries(dir)?;
    let mut files = vec![dir.join("corpus.arrow")];
    if state == "appended" {
        files.push(dir.join("append.arrow"));
    }
    let mut variants: Vec<(u32, Option<u8>, u64)> = Vec::new();
    for selectivity in SELECTIVITIES {
        let code = Filter::for_selectivity(selectivity)?.group.map(group_code);
        variants.extend(LIVE_PERCENTS.iter().map(|live| (selectivity, code, *live)));
    }

    let query_terms: Vec<Vec<String>> = queries
        .iter()
        .map(|q| {
            let mut terms: Vec<String> = tokens(&q.text).collect();
            terms.sort_unstable();
            terms.dedup();
            terms
        })
        .collect();
    let wanted: AHashSet<&str> = query_terms.iter().flatten().map(String::as_str).collect();

    let mut nearest: Vec<Vec<Vec<(f32, u64)>>> = vec![vec![Vec::new(); variants.len()]; queries.len()];
    let mut postings: AHashMap<String, Vec<(u64, f32, f32)>> = AHashMap::new();
    let mut groups: Vec<u8> = Vec::new();
    let (mut row, mut total_tokens) = (0u64, 0u64);
    for file in &files {
        for batch in open_ipc(file)? {
            let batch = batch?;
            let columns = Columns::of(&batch)?;
            let len = batch.num_rows();
            ensure!(
                columns.doc_key.value(0) == doc_key(params.seed, row / CHUNKS_PER_DOC),
                "corpus rows are not in generation order at row {row}"
            );
            let codes: Vec<u8> = (0..len).map(|i| group_code(columns.mime_group.value(i))).collect();
            nearest.par_iter_mut().zip(&queries).for_each(|(per_variant, query)| {
                for i in 0..len {
                    let global = row + i as u64;
                    let distance = l2(&query.vector, &columns.vectors[i * params.dim..(i + 1) * params.dim]);
                    for (best, (_, code, live)) in per_variant.iter_mut().zip(&variants) {
                        if code.is_none_or(|c| c == codes[i]) && is_live(params.seed, global / CHUNKS_PER_DOC, *live) {
                            keep_best(best, (distance, global), |a, b| a < b);
                        }
                    }
                }
            });
            let matched: Vec<RowTerms> = (0..len)
                .into_par_iter()
                .map(|i| {
                    let mut tf: AHashMap<String, f32> = AHashMap::new();
                    let mut doc_len = 0.0;
                    for token in tokens(columns.text.value(i)) {
                        doc_len += 1.0;
                        if wanted.contains(token.as_str()) {
                            *tf.entry(token).or_default() += 1.0;
                        }
                    }
                    let hits = tf.into_iter().map(|(term, count)| (term, count, doc_len)).collect();
                    (doc_len as u64, hits)
                })
                .collect();
            for (i, (doc_len, hits)) in matched.into_iter().enumerate() {
                total_tokens += doc_len;
                for (term, count, len) in hits {
                    postings.entry(term).or_default().push((row + i as u64, count, len));
                }
            }
            groups.extend(codes);
            row += len as u64;
        }
    }

    let mut entries = BTreeMap::new();
    for (at, (selectivity, _, live)) in variants.iter().enumerate() {
        let ids = nearest
            .iter()
            .map(|per_variant| per_variant[at].iter().map(|(_, r)| render(params.seed, *r)).collect())
            .collect();
        entries.insert(variant("vector", *selectivity, *live), ids);
    }
    let avg_len = total_tokens as f32 / row.max(1) as f32;
    for (selectivity, code, live) in variants {
        let ids = query_terms
            .par_iter()
            .map(|terms| {
                let mut scores: AHashMap<u64, f32> = AHashMap::new();
                for term in terms {
                    let Some(list) = postings.get(term) else { continue };
                    for (r, tf, doc_len) in list {
                        if code.is_none_or(|c| c == groups[*r as usize])
                            && is_live(params.seed, *r / CHUNKS_PER_DOC, live)
                        {
                            *scores.entry(*r).or_default() += bm25(*tf, *doc_len, avg_len, list.len(), row);
                        }
                    }
                }
                let mut ranked: Vec<(u64, f32)> = scores.into_iter().collect();
                ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                ranked
                    .iter()
                    .take(TRUTH_K)
                    .map(|(r, _)| render(params.seed, *r))
                    .collect()
            })
            .collect();
        entries.insert(variant("lexical", selectivity, live), ids);
    }
    Ok(Truth {
        state: state.to_string(),
        rows: row,
        entries,
    })
}

/// `(recall@10, nDCG@10)` of one ranked answer, through the product's own scorer.
pub fn quality(returned: &[Hit], gold: &[String]) -> (f64, f64) {
    let returned: Vec<Item> = returned.iter().map(|hit| Item::parse(&hit.render())).collect();
    let gold: Vec<Item> = gold.iter().map(|id| Item::parse(id)).collect();
    let scored = score(&returned, &gold, 0, Some(TRUTH_K));
    (scored.recall, scored.ndcg.unwrap_or(0.0))
}

/// The live-document keys of the manifest that marks `percent` of documents live.
pub fn live_keys(params: &Params, rows: u64, percent: u64) -> Vec<String> {
    (0..rows.div_ceil(CHUNKS_PER_DOC))
        .filter(|doc| is_live(params.seed, *doc, percent))
        .map(|doc| doc_key(params.seed, doc))
        .collect()
}

/// Liveness after ranking, for a key list too long to be a predicate (ADR-0029 rule 3): over-fetch,
/// keep live hits, widen while short.
/// Returns the hits, the mode of the last fetch, and whether the cap was hit while still short.
pub fn search_live(
    backend: &dyn Backend,
    query: &[f32],
    filter: &Filter,
    k: usize,
    live: &AHashSet<String>,
) -> Result<(Vec<Hit>, VectorMode, bool)> {
    let mut fetch = (3 * k).max(k + 64);
    loop {
        let (hits, mode) = backend.vector(query, filter, fetch)?;
        let exhausted = hits.len() < fetch;
        let mut kept: Vec<Hit> = hits.into_iter().filter(|hit| live.contains(&hit.doc_key)).collect();
        if kept.len() >= k || exhausted || fetch >= LIVE_FETCH_CAP {
            let short = kept.len() < k && !exhausted;
            kept.truncate(k);
            return Ok((kept, mode, short));
        }
        fetch = (fetch * 2).min(LIVE_FETCH_CAP);
    }
}

/// Nearest-rank percentile of an ascending slice, as `src/eval/report.rs` computes it.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}
