//! Pure scoring: a ranked list of returned [`Item`]s against a gold set.
//!
//! Matching: a returned item hits a gold entry when the paths are equal and, if the gold entry
//! names a line, the lines are within `slack`. Returned items are de-duplicated first, at line
//! granularity when any gold entry names a line and at path granularity otherwise, so a tool
//! that lists ten hits in one file is not punished (or rewarded) ten times for a path-level gold.
//!
//! Conventions for degenerate input: with an empty gold set recall is 1.0 (nothing to miss) and
//! precision is 1.0 only for an empty answer, else 0.0. With a non-empty gold and an empty answer
//! precision is 0.0 (nothing returned is correct) and recall 0.0.

use serde::{Deserialize, Serialize};

/// A returned or expected location.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Item {
    pub path: String,
    pub line: Option<u32>,
}

impl Item {
    /// Parse `path` or `path:line` (the suffix counts as a line only when it is all digits).
    pub fn parse(s: &str) -> Self {
        if let Some((p, l)) = s.rsplit_once(':')
            && !p.is_empty()
            && !l.is_empty()
            && l.bytes().all(|b| b.is_ascii_digit())
            && let Ok(line) = l.parse()
        {
            return Self {
                path: p.to_string(),
                line: Some(line),
            };
        }
        Self {
            path: s.to_string(),
            line: None,
        }
    }
}

/// Scores for one task.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Score {
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
    /// Ranked tasks only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_at_1: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_at_5: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_at_k: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mrr: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ndcg: Option<f64>,
    /// Distinct returned items after de-duplication (and `k`-truncation for ranked tasks).
    pub returned: usize,
    pub gold: usize,
}

fn hits(gold: &Item, got: &Item, slack: u32) -> bool {
    gold.path == got.path
        && match (gold.line, got.line) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(g), Some(r)) => g.abs_diff(r) <= slack,
        }
}

fn dedupe(returned: &[Item], line_level: bool) -> Vec<Item> {
    let mut seen = std::collections::HashSet::new();
    returned
        .iter()
        .filter(|i| {
            let key = (i.path.clone(), if line_level { i.line } else { None });
            seen.insert(key)
        })
        .cloned()
        .collect()
}

fn f1(p: f64, r: f64) -> f64 {
    if p + r == 0.0 { 0.0 } else { 2.0 * p * r / (p + r) }
}

/// Score `returned` (in rank order) against `gold`. `ranked_k` is `Some(k)` for ranked scoring.
pub fn score(returned: &[Item], gold: &[Item], slack: u32, ranked_k: Option<usize>) -> Score {
    let line_level = gold.iter().any(|g| g.line.is_some());
    let mut got = dedupe(returned, line_level);
    if let Some(k) = ranked_k {
        got.truncate(k);
    }
    // For each returned rank: index of the first not-yet-found gold entry it satisfies.
    let mut found = vec![false; gold.len()];
    let mut rel_rank: Vec<bool> = Vec::with_capacity(got.len());
    let mut relevant_returned = 0usize;
    for g in &got {
        let any = gold.iter().any(|x| hits(x, g, slack));
        if any {
            relevant_returned += 1;
        }
        let fresh = gold.iter().enumerate().find(|(i, x)| !found[*i] && hits(x, g, slack));
        if let Some((i, _)) = fresh {
            found[i] = true;
        }
        rel_rank.push(fresh.is_some());
    }
    let found_gold = gold.iter().filter(|x| got.iter().any(|g| hits(x, g, slack))).count();

    let (precision, recall) = if gold.is_empty() {
        (if got.is_empty() { 1.0 } else { 0.0 }, 1.0)
    } else {
        (
            if got.is_empty() {
                0.0
            } else {
                relevant_returned as f64 / got.len() as f64
            },
            found_gold as f64 / gold.len() as f64,
        )
    };
    let mut s = Score {
        precision,
        recall,
        f1: f1(precision, recall),
        returned: got.len(),
        gold: gold.len(),
        ..Score::default()
    };
    if let Some(k) = ranked_k {
        let first = |n: usize| rel_rank.iter().take(n).any(|r| *r);
        let b = |v: bool| if v { 1.0 } else { 0.0 };
        s.hit_at_1 = Some(b(first(1)));
        s.hit_at_5 = Some(b(first(5)));
        s.hit_at_k = Some(b(first(k)));
        s.mrr = Some(rel_rank.iter().position(|r| *r).map_or(0.0, |p| 1.0 / (p as f64 + 1.0)));
        // `fold` from +0.0: an empty `sum()` of f64 is -0.0, which would print as `-0.0`.
        let discount = |i: usize| 1.0 / (i as f64 + 2.0).log2();
        let dcg = rel_rank
            .iter()
            .enumerate()
            .filter(|(_, r)| **r)
            .fold(0.0, |acc, (i, _)| acc + discount(i));
        let ideal = (0..gold.len().min(k)).fold(0.0, |acc, i| acc + discount(i));
        s.ndcg = Some(if ideal == 0.0 { 1.0 } else { dcg / ideal });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn it(s: &str) -> Item {
        Item::parse(s)
    }
    fn items(v: &[&str]) -> Vec<Item> {
        v.iter().map(|s| it(s)).collect()
    }
    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn parses_path_and_line() {
        assert_eq!(
            it("a/b.py:12"),
            Item {
                path: "a/b.py".into(),
                line: Some(12)
            }
        );
        assert_eq!(it("a/b.py").line, None);
        assert_eq!(it("a:b.py").line, None);
        assert_eq!(it("x:y:7").path, "x:y");
    }

    #[test]
    fn perfect_partial_and_empty_sets() {
        let p = score(&items(&["a", "b"]), &items(&["a", "b"]), 0, None);
        assert_eq!((p.precision, p.recall, p.f1), (1.0, 1.0, 1.0));

        // 1 of 2 returned is right, 1 of 4 gold found.
        let s = score(&items(&["a", "x"]), &items(&["a", "b", "c", "d"]), 0, None);
        close(s.precision, 0.5);
        close(s.recall, 0.25);
        close(s.f1, 2.0 * 0.5 * 0.25 / 0.75);

        let miss = score(&[], &items(&["a"]), 0, None);
        assert_eq!((miss.precision, miss.recall, miss.f1), (0.0, 0.0, 0.0));

        assert_eq!(score(&[], &[], 0, None).f1, 1.0);
        assert_eq!(score(&items(&["a"]), &[], 0, None).f1, 0.0);
    }

    #[test]
    fn duplicates_collapse_at_path_level_but_not_line_level() {
        let path_gold = score(&items(&["a:1", "a:2", "a:9"]), &items(&["a"]), 0, None);
        assert_eq!(path_gold.returned, 1);
        assert_eq!(path_gold.precision, 1.0);
        let line_gold = score(&items(&["a:1", "a:2", "a:9"]), &items(&["a:2"]), 0, None);
        assert_eq!(line_gold.returned, 3);
        close(line_gold.precision, 1.0 / 3.0);
        assert_eq!(line_gold.recall, 1.0);
    }

    #[test]
    fn line_slack_and_missing_line() {
        assert_eq!(score(&items(&["a:12"]), &items(&["a:10"]), 1, None).recall, 0.0);
        assert_eq!(score(&items(&["a:11"]), &items(&["a:10"]), 1, None).recall, 1.0);
        // A path-only answer cannot satisfy a line-level gold.
        assert_eq!(score(&items(&["a"]), &items(&["a:10"]), 5, None).recall, 0.0);
    }

    #[test]
    fn ranked_metrics() {
        // First relevant at rank 3; gold has two entries, second never found.
        let s = score(&items(&["x", "y", "a"]), &items(&["a", "b"]), 0, Some(5));
        assert_eq!(s.hit_at_1, Some(0.0));
        assert_eq!(s.hit_at_5, Some(1.0));
        close(s.mrr.unwrap(), 1.0 / 3.0);
        let dcg = 1.0 / 4f64.log2();
        let ideal = 1.0 + 1.0 / 3f64.log2();
        close(s.ndcg.unwrap(), dcg / ideal);

        let perfect = score(&items(&["a", "b"]), &items(&["a", "b"]), 0, Some(5));
        assert_eq!(
            (perfect.mrr, perfect.ndcg, perfect.hit_at_1),
            (Some(1.0), Some(1.0), Some(1.0))
        );

        let none = score(&items(&["x"]), &items(&["a"]), 0, Some(5));
        assert_eq!((none.mrr, none.ndcg, none.hit_at_k), (Some(0.0), Some(0.0), Some(0.0)));
    }

    #[test]
    fn ranked_truncates_to_k() {
        // Relevant item sits at rank 3 but k = 2, so it is cut off.
        let s = score(&items(&["x", "y", "a"]), &items(&["a"]), 0, Some(2));
        assert_eq!(s.returned, 2);
        assert_eq!(s.recall, 0.0);
        assert_eq!(s.mrr, Some(0.0));
    }

    #[test]
    fn set_scoring_has_no_rank_metrics() {
        let s = score(&items(&["a"]), &items(&["a"]), 0, None);
        assert!(s.mrr.is_none() && s.ndcg.is_none() && s.hit_at_1.is_none());
    }
}
