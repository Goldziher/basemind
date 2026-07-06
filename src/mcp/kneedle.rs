//! Knee/elbow cutoff for a descending score curve.
//!
//! Given scores sorted in DESCENDING order, returns how many leading items to keep:
//! the cutoff falls at the "knee" where the curve stops dropping steeply and flattens
//! — the natural relevance cliff. Used to trim a ranked result list to its significant
//! head instead of an arbitrary fixed N.
//!
//! Implementation: the difference-curve knee of Satopaa 2011 ("Kneedle"), simplified
//! for a monotone-decreasing input. Both axes are min-max normalized to `[0, 1]`, then
//! the knee is the point of maximum `y - x` on the normalized difference curve — the
//! elbow where scores have dropped most relative to how far down the list we are.
//! Deterministic, allocation-free, `O(n)`.

/// Number of leading items to keep from a DESCENDING-sorted `scores` slice.
///
/// Returns a 1-based count (the index just past the knee). Degenerate inputs — fewer
/// than three points, or a flat / straight-line curve with no distinguishable knee —
/// keep everything.
pub(super) fn knee_cutoff(scores: &[f64]) -> usize {
    let n = scores.len();
    if n < 3 {
        return n;
    }
    let first = scores[0];
    let last = scores[n - 1];
    let span = first - last;
    // Flat (or non-decreasing) curve: no meaningful knee, keep all.
    if span <= f64::EPSILON {
        return n;
    }
    let x_span = (n - 1) as f64;
    let mut best_idx = 0usize;
    let mut best_dist = f64::NEG_INFINITY;
    for (i, &s) in scores.iter().enumerate() {
        // x: position normalized 0..1. y: drop-from-head normalized 0..1 (0 at the
        // head, 1 at the tail). For a convex-decreasing curve `y` rises fast then
        // flattens, so `y - x` peaks at the knee.
        let x = i as f64 / x_span;
        let y = (first - s) / span;
        let dist = y - x;
        if dist > best_dist {
            best_dist = dist;
            best_idx = i;
        }
    }
    // No point rises above the diagonal → a straight-ish decline with no elbow; keep all.
    if best_dist <= 0.0 {
        return n;
    }
    best_idx + 1
}

#[cfg(test)]
mod tests {
    use super::knee_cutoff;

    #[test]
    fn fewer_than_three_points_keeps_all() {
        assert_eq!(knee_cutoff(&[]), 0);
        assert_eq!(knee_cutoff(&[5.0]), 1);
        assert_eq!(knee_cutoff(&[5.0, 1.0]), 2);
    }

    #[test]
    fn flat_curve_keeps_all() {
        assert_eq!(knee_cutoff(&[5.0, 5.0, 5.0, 5.0]), 4);
    }

    #[test]
    fn straight_line_decline_keeps_all() {
        // y == x everywhere → no elbow.
        assert_eq!(knee_cutoff(&[5.0, 4.0, 3.0, 2.0, 1.0]), 5);
    }

    #[test]
    fn sharp_elbow_cuts_at_the_knee() {
        // Two dominant hubs then a long flat tail: the knee sits at the corner.
        assert_eq!(knee_cutoff(&[100.0, 90.0, 5.0, 4.0, 3.0, 2.0]), 3);
    }

    #[test]
    fn single_dominant_head() {
        // One outlier, then a flat floor.
        let keep = knee_cutoff(&[1000.0, 10.0, 9.0, 8.0, 7.0, 6.0]);
        assert_eq!(keep, 2, "should keep just the dominant head + the corner");
    }
}
