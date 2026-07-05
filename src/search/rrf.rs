//! Reciprocal Rank Fusion (RRF) for hybrid code search.
//!
//! Fuses the independently-ranked lanes of [`crate::search`] — vector (semantic), keyword (BM25),
//! and exact (symbol) — into one ranking without needing their scores to be comparable. Each lane
//! contributes `weight / (k + rank)` to every chunk it ranks (rank is 1-based), and a chunk's fused
//! score is the sum across lanes. RRF is score-scale-agnostic (it only reads ranks), which is why it
//! can blend an L2 distance, a BM25 score, and a symbol match order without normalization.
//!
//! The universal join key is `chunk_id` (`<source-hash-hex>:<ordinal>`), emitted by every lane.

use std::cmp::Ordering;

use ahash::AHashMap;

/// The RRF rank-damping constant. 60 is the value from the original Cormack et al. paper and the
/// de-facto default across search stacks — large enough that the top few ranks of each lane stay
/// close in contribution, so no single lane dominates on rank-1 alone.
pub const DEFAULT_RRF_K: f32 = 60.0;

/// Weight for the exact/symbol lane. Higher than the others because an identifier-shaped query that
/// matches a defined symbol is a high-precision signal — the chunk that *defines* the symbol should
/// win ties against a merely lexical or semantic co-occurrence.
pub const WEIGHT_EXACT: f32 = 2.0;
/// Weight for the vector (semantic) lane.
pub const WEIGHT_VECTOR: f32 = 1.0;
/// Weight for the keyword (BM25) lane.
pub const WEIGHT_KEYWORD: f32 = 1.0;

/// One ranked lane's contribution to the fusion: its chunk ids (best-first) and its weight.
pub struct FusionLane<'a> {
    /// Chunk ids in rank order, best first. Duplicates within a lane are ignored after the first.
    pub chunk_ids: &'a [String],
    /// Lane weight — scales this lane's `1 / (k + rank)` contribution.
    pub weight: f32,
}

impl<'a> FusionLane<'a> {
    /// Construct a lane from a ranked slice and a weight.
    pub fn new(chunk_ids: &'a [String], weight: f32) -> Self {
        Self { chunk_ids, weight }
    }
}

/// Fuse ranked lanes via RRF. Returns `(chunk_id, fused_score)` sorted by score descending, with a
/// stable ascending-`chunk_id` tie-break so the order is deterministic across runs. Empty lanes (and
/// an empty `lanes` slice) contribute nothing.
pub fn rrf_fuse(lanes: &[FusionLane<'_>], k: f32) -> Vec<(String, f32)> {
    let mut scores: AHashMap<&str, f32> = AHashMap::new();
    for lane in lanes {
        // Guard against a duplicate chunk_id within a single lane inflating its own contribution —
        // only the best (first) rank of a chunk within a lane counts.
        let mut seen_in_lane: AHashMap<&str, ()> = AHashMap::new();
        for (rank0, chunk_id) in lane.chunk_ids.iter().enumerate() {
            if seen_in_lane.insert(chunk_id.as_str(), ()).is_some() {
                continue;
            }
            let rank = (rank0 + 1) as f32;
            *scores.entry(chunk_id.as_str()).or_insert(0.0) += lane.weight / (k + rank);
        }
    }
    let mut fused: Vec<(String, f32)> = scores.into_iter().map(|(id, s)| (id.to_string(), s)).collect();
    fused.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn agreeing_lanes_rank_the_shared_top_first() {
        let a = ids(&["h:1", "h:2", "h:3"]);
        let b = ids(&["h:1", "h:3", "h:4"]);
        let fused = rrf_fuse(&[FusionLane::new(&a, 1.0), FusionLane::new(&b, 1.0)], DEFAULT_RRF_K);
        // h:1 is rank-1 in both lanes → strictly highest fused score.
        assert_eq!(fused[0].0, "h:1");
        assert!(fused[0].1 > fused[1].1);
        // Every chunk id from either lane appears exactly once.
        let uniq: std::collections::HashSet<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(uniq.len(), 4);
    }

    #[test]
    fn weight_boosts_a_lane_that_ranks_a_chunk() {
        let exact = ids(&["h:def"]);
        let keyword = ids(&["h:other", "h:def"]);
        // With a heavy exact weight, the exact-lane's sole hit outranks the keyword-lane's rank-1.
        let fused = rrf_fuse(
            &[
                FusionLane::new(&exact, WEIGHT_EXACT),
                FusionLane::new(&keyword, WEIGHT_KEYWORD),
            ],
            DEFAULT_RRF_K,
        );
        assert_eq!(fused[0].0, "h:def", "exact-lane rank-1 with 2x weight must win");
    }

    #[test]
    fn duplicate_within_lane_counts_once() {
        let dupe = ids(&["h:1", "h:1", "h:1"]);
        let single = ids(&["h:1"]);
        let a = rrf_fuse(&[FusionLane::new(&dupe, 1.0)], DEFAULT_RRF_K);
        let b = rrf_fuse(&[FusionLane::new(&single, 1.0)], DEFAULT_RRF_K);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].1, b[0].1, "repeats of a chunk within a lane must not stack");
    }

    #[test]
    fn empty_lanes_produce_empty_output() {
        let empty: Vec<String> = Vec::new();
        assert!(rrf_fuse(&[FusionLane::new(&empty, 1.0)], DEFAULT_RRF_K).is_empty());
        assert!(rrf_fuse(&[], DEFAULT_RRF_K).is_empty());
    }

    #[test]
    fn equal_scores_break_ties_by_chunk_id_ascending() {
        // Two lanes, disjoint single hits at the same rank → equal scores → deterministic id order.
        let a = ids(&["h:zzz"]);
        let b = ids(&["h:aaa"]);
        let fused = rrf_fuse(&[FusionLane::new(&a, 1.0), FusionLane::new(&b, 1.0)], DEFAULT_RRF_K);
        assert_eq!(fused[0].0, "h:aaa");
        assert_eq!(fused[1].0, "h:zzz");
    }
}
