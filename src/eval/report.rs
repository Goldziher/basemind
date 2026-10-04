//! Aggregation of per-task results into the per-mode report (JSON + markdown) and the
//! baseline-report regression comparison.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::score::Score;
use super::task::EvalMode;

/// Modes whose measured median savings ratio differs from the `savings.rs` multiplier by more than
/// this fraction are flagged.
pub const DEVIATION_FLAG: f64 = 0.25;

/// The baseline-command side of one task.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SavingsOutcome {
    pub baseline_tokens: u64,
    pub baseline_elapsed_us: u64,
    /// `baseline_tokens / basemind_tokens`; absent when the basemind answer is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
    /// `baseline_tokens - basemind_tokens`, may be negative.
    pub saved_tokens: i64,
    /// Recall met the threshold, so the savings count.
    pub credited: bool,
    /// What `savings.rs` would assume for the same response, as `baseline / actual`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model_label: String,
}

/// One NDJSON row.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskResult {
    pub id: String,
    pub mode: EvalMode,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub elapsed_us: u64,
    /// Tokens of the response text an agent would receive.
    pub tokens: u64,
    pub score: Score,
    /// Ranked only: whether this task used rank metrics.
    pub ranked: bool,
    /// Up to the first 20 returned items (`path` or `path:line`), for debugging misses.
    pub returned: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub savings: Option<SavingsOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_error: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Dist {
    pub median: f64,
    pub p90: f64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ModeSavings {
    /// Tasks with a baseline whose savings counted / were withheld for low recall.
    pub credited: usize,
    pub uncredited: usize,
    pub ratio: Dist,
    pub saved_tokens: Dist,
    pub baseline_tokens_total: u64,
    pub basemind_tokens_total: u64,
    /// `savings.rs` multiplier (baseline/actual) and the baseline name it labels it with.
    pub model_ratio: Option<f64>,
    pub model_label: String,
    /// `(measured median ratio - model) / model`.
    pub deviation: Option<f64>,
    /// `|deviation| > 25%`.
    pub flagged: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ModeReport {
    pub tasks: usize,
    pub errors: usize,
    pub latency_p50_us: u64,
    pub latency_p95_us: u64,
    pub mean_precision: f64,
    pub mean_recall: f64,
    pub mean_f1: f64,
    /// Means over the ranked tasks only; `None` when the mode has none.
    pub ranked_tasks: usize,
    pub hit_at_1: Option<f64>,
    pub hit_at_5: Option<f64>,
    pub mrr: Option<f64>,
    pub ndcg: Option<f64>,
    pub mean_tokens: f64,
    pub savings: Option<ModeSavings>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Report {
    pub version: u32,
    /// `o200k` or `bytes/4` — the tokenizer both sides were counted with.
    pub tokenizer: String,
    pub root: String,
    pub tasks_file: String,
    pub min_recall: f64,
    pub modes: BTreeMap<EvalMode, ModeReport>,
    pub overall: ModeReport,
}

/// Nearest-rank percentile of an ascending slice (`p` in 0..=100); 0 for an empty slice.
pub fn percentile<T: Copy + Default>(sorted: &[T], p: f64) -> T {
    if sorted.is_empty() {
        return T::default();
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn mean(v: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = v.fold((0.0, 0usize), |(s, n), x| (s + x, n + 1));
    (n > 0).then(|| sum / n as f64)
}

fn dist(mut v: Vec<f64>) -> Dist {
    v.sort_by(f64::total_cmp);
    Dist {
        median: percentile(&v, 50.0),
        p90: percentile(&v, 90.0),
    }
}

fn savings_for(results: &[&TaskResult]) -> Option<ModeSavings> {
    let with: Vec<(&TaskResult, &SavingsOutcome)> = results
        .iter()
        .filter_map(|r| r.savings.as_ref().map(|s| (*r, s)))
        .collect();
    if with.is_empty() {
        return None;
    }
    let credited: Vec<_> = with.iter().filter(|(_, s)| s.credited).collect();
    let ratios: Vec<f64> = credited.iter().filter_map(|(_, s)| s.ratio).collect();
    let saved: Vec<f64> = credited.iter().map(|(_, s)| s.saved_tokens as f64).collect();
    let mut models: Vec<f64> = with.iter().filter_map(|(_, s)| s.model_ratio).collect();
    models.sort_by(f64::total_cmp);
    let model_ratio = (!models.is_empty()).then(|| percentile(&models, 50.0));
    let ratio = dist(ratios);
    let deviation = match (model_ratio, credited.is_empty()) {
        (Some(m), false) if m > 0.0 => Some((ratio.median - m) / m),
        _ => None,
    };
    Some(ModeSavings {
        credited: credited.len(),
        uncredited: with.len() - credited.len(),
        ratio,
        saved_tokens: dist(saved),
        baseline_tokens_total: credited.iter().map(|(_, s)| s.baseline_tokens).sum(),
        basemind_tokens_total: credited.iter().map(|(r, _)| r.tokens).sum(),
        model_ratio,
        model_label: with
            .iter()
            .map(|(_, s)| s.model_label.clone())
            .find(|l| !l.is_empty())
            .unwrap_or_default(),
        deviation,
        flagged: deviation.is_some_and(|d| d.abs() > DEVIATION_FLAG),
    })
}

fn mode_report(results: &[&TaskResult]) -> ModeReport {
    let mut lat: Vec<u64> = results.iter().map(|r| r.elapsed_us).collect();
    lat.sort_unstable();
    let ranked: Vec<&&TaskResult> = results.iter().filter(|r| r.ranked).collect();
    let m = |f: fn(&Score) -> f64| mean(results.iter().map(|r| f(&r.score))).unwrap_or(0.0);
    let rm = |f: fn(&Score) -> Option<f64>| mean(ranked.iter().filter_map(|r| f(&r.score)));
    ModeReport {
        tasks: results.len(),
        errors: results.iter().filter(|r| !r.ok).count(),
        latency_p50_us: percentile(&lat, 50.0),
        latency_p95_us: percentile(&lat, 95.0),
        mean_precision: m(|s| s.precision),
        mean_recall: m(|s| s.recall),
        mean_f1: m(|s| s.f1),
        ranked_tasks: ranked.len(),
        hit_at_1: rm(|s| s.hit_at_1),
        hit_at_5: rm(|s| s.hit_at_5),
        mrr: rm(|s| s.mrr),
        ndcg: rm(|s| s.ndcg),
        mean_tokens: mean(results.iter().map(|r| r.tokens as f64)).unwrap_or(0.0),
        savings: savings_for(results),
    }
}

pub fn aggregate(results: &[TaskResult], tokenizer: &str, root: &str, tasks_file: &str, min_recall: f64) -> Report {
    let mut by_mode: BTreeMap<EvalMode, Vec<&TaskResult>> = BTreeMap::new();
    for r in results {
        by_mode.entry(r.mode).or_default().push(r);
    }
    let all: Vec<&TaskResult> = results.iter().collect();
    Report {
        version: 1,
        tokenizer: tokenizer.to_string(),
        root: root.to_string(),
        tasks_file: tasks_file.to_string(),
        min_recall,
        modes: by_mode.iter().map(|(m, v)| (*m, mode_report(v))).collect(),
        overall: mode_report(&all),
    }
}

fn pct(v: Option<f64>) -> String {
    v.map_or_else(|| "-".to_string(), |x| format!("{x:.3}"))
}

pub fn markdown(r: &Report) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# basemind eval\n");
    let _ = writeln!(
        s,
        "root `{}`, tasks `{}`, tokenizer `{}`, savings credited at recall >= {:.2}\n",
        r.root, r.tasks_file, r.tokenizer, r.min_recall
    );
    let _ = writeln!(s, "## Retrieval quality\n");
    let _ = writeln!(
        s,
        "| mode | tasks | err | p50 us | p95 us | P | R | F1 | hit@1 | hit@5 | MRR | nDCG | tokens |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    let row = |s: &mut String, name: &str, m: &ModeReport| {
        let _ = writeln!(
            s,
            "| {name} | {} | {} | {} | {} | {:.3} | {:.3} | {:.3} | {} | {} | {} | {} | {:.0} |",
            m.tasks,
            m.errors,
            m.latency_p50_us,
            m.latency_p95_us,
            m.mean_precision,
            m.mean_recall,
            m.mean_f1,
            pct(m.hit_at_1),
            pct(m.hit_at_5),
            pct(m.mrr),
            pct(m.ndcg),
            m.mean_tokens
        );
    };
    for (mode, m) in &r.modes {
        row(&mut s, mode.as_str(), m);
    }
    row(&mut s, "**all**", &r.overall);
    let with_savings: Vec<_> = r
        .modes
        .iter()
        .filter_map(|(k, m)| m.savings.as_ref().map(|v| (k, v)))
        .collect();
    if !with_savings.is_empty() {
        let _ = writeln!(s, "\n## Token savings vs `savings.rs` multipliers\n");
        let _ = writeln!(
            s,
            "| mode | credited | withheld | median x | p90 x | median saved | p90 saved | model x | model | deviation | flag |"
        );
        let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|---|");
        for (mode, v) in with_savings {
            let _ = writeln!(
                s,
                "| {} | {} | {} | {:.2} | {:.2} | {:.0} | {:.0} | {} | {} | {} | {} |",
                mode.as_str(),
                v.credited,
                v.uncredited,
                v.ratio.median,
                v.ratio.p90,
                v.saved_tokens.median,
                v.saved_tokens.p90,
                v.model_ratio.map_or_else(|| "-".into(), |x| format!("{x:.2}")),
                v.model_label,
                v.deviation
                    .map_or_else(|| "-".into(), |d| format!("{:+.0}%", d * 100.0)),
                if v.flagged { "DEVIATES" } else { "" }
            );
        }
    }
    s
}

#[derive(Debug, Clone, PartialEq)]
pub struct Regression {
    pub mode: String,
    pub metric: &'static str,
    pub baseline: f64,
    pub current: f64,
}

impl std::fmt::Display for Regression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}: {:.4} -> {:.4}",
            self.mode, self.metric, self.baseline, self.current
        )
    }
}

/// Latency differences below this many microseconds are noise, whatever the ratio.
const LATENCY_NOISE_FLOOR_US: f64 = 1000.0;

fn compare_mode(
    name: &str,
    base: &ModeReport,
    cur: &ModeReport,
    quality_tol: f64,
    cost_tol: f64,
    out: &mut Vec<Regression>,
) {
    let mut push = |metric, b: f64, c: f64| {
        out.push(Regression {
            mode: name.to_string(),
            metric,
            baseline: b,
            current: c,
        })
    };
    let quality: [(&'static str, Option<f64>, Option<f64>); 6] = [
        ("mean_f1", Some(base.mean_f1), Some(cur.mean_f1)),
        ("mean_recall", Some(base.mean_recall), Some(cur.mean_recall)),
        ("hit_at_1", base.hit_at_1, cur.hit_at_1),
        ("hit_at_5", base.hit_at_5, cur.hit_at_5),
        ("mrr", base.mrr, cur.mrr),
        ("ndcg", base.ndcg, cur.ndcg),
    ];
    for (metric, b, c) in quality {
        match (b, c) {
            (Some(b), Some(c)) if b - c > quality_tol => push(metric, b, c),
            (Some(b), None) => push(metric, b, 0.0),
            _ => {}
        }
    }
    let (bl, cl) = (base.latency_p95_us as f64, cur.latency_p95_us as f64);
    if bl > 0.0 && cl - bl > LATENCY_NOISE_FLOOR_US && (cl - bl) / bl > cost_tol {
        push("latency_p95_us", bl, cl);
    }
    if base.mean_tokens > 0.0 && (cur.mean_tokens - base.mean_tokens) / base.mean_tokens > cost_tol {
        push("mean_tokens", base.mean_tokens, cur.mean_tokens);
    }
    if cur.errors > base.errors {
        push("errors", base.errors as f64, cur.errors as f64);
    }
}

/// Regressions of `cur` against `base`. Quality metrics regress on an absolute drop greater than
/// `quality_tol`; latency p95 and mean tokens on a relative increase greater than `cost_tol`.
/// A mode present in the baseline but absent now is a regression.
pub fn compare(base: &Report, cur: &Report, quality_tol: f64, cost_tol: f64) -> Vec<Regression> {
    let mut out = Vec::new();
    for (mode, b) in &base.modes {
        match cur.modes.get(mode) {
            Some(c) => compare_mode(mode.as_str(), b, c, quality_tol, cost_tol, &mut out),
            None => out.push(Regression {
                mode: mode.as_str().to_string(),
                metric: "tasks",
                baseline: b.tasks as f64,
                current: 0.0,
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str, mode: EvalMode, f1: f64, ranked: bool, us: u64, tokens: u64) -> TaskResult {
        TaskResult {
            id: id.into(),
            mode,
            ok: true,
            error: None,
            elapsed_us: us,
            tokens,
            score: Score {
                precision: f1,
                recall: f1,
                f1,
                hit_at_1: ranked.then_some(f1),
                hit_at_5: ranked.then_some(f1),
                mrr: ranked.then_some(f1),
                ndcg: ranked.then_some(f1),
                ..Score::default()
            },
            ranked,
            returned: vec![],
            savings: None,
            baseline_error: None,
        }
    }

    fn with_savings(mut r: TaskResult, baseline: u64, credited: bool, model: f64) -> TaskResult {
        r.savings = Some(SavingsOutcome {
            baseline_tokens: baseline,
            ratio: Some(baseline as f64 / r.tokens as f64),
            saved_tokens: baseline as i64 - r.tokens as i64,
            credited,
            model_ratio: Some(model),
            model_label: "grep_top_hits".into(),
            ..SavingsOutcome::default()
        });
        r
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let v = [10u64, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(percentile(&v, 50.0), 50);
        assert_eq!(percentile(&v, 95.0), 100);
        assert_eq!(percentile(&v, 90.0), 90);
        assert_eq!(percentile::<u64>(&[], 50.0), 0);
    }

    #[test]
    fn aggregates_means_and_ranked_only_metrics() {
        let rs = vec![
            result("a", EvalMode::Symbols, 1.0, false, 100, 10),
            result("b", EvalMode::Symbols, 0.5, false, 300, 30),
            result("c", EvalMode::Find, 1.0, true, 50, 5),
            result("d", EvalMode::Find, 0.0, true, 70, 7),
        ];
        let rep = aggregate(&rs, "o200k", "/r", "t.jsonl", 0.8);
        let s = &rep.modes[&EvalMode::Symbols];
        assert_eq!(s.tasks, 2);
        assert!((s.mean_f1 - 0.75).abs() < 1e-9);
        assert_eq!(s.latency_p50_us, 100);
        assert_eq!(s.latency_p95_us, 300);
        assert!(s.mrr.is_none(), "set-only mode has no rank metrics");
        assert!((s.mean_tokens - 20.0).abs() < 1e-9);
        let f = &rep.modes[&EvalMode::Find];
        assert_eq!(f.ranked_tasks, 2);
        assert!((f.mrr.unwrap() - 0.5).abs() < 1e-9);
        assert_eq!(rep.overall.tasks, 4);
    }

    #[test]
    fn savings_only_count_credited_tasks_and_flag_deviation() {
        let rs = vec![
            with_savings(result("a", EvalMode::Symbols, 1.0, false, 1, 100), 1000, true, 3.0),
            with_savings(result("b", EvalMode::Symbols, 1.0, false, 1, 100), 500, true, 3.0),
            // Would drag the median to 5x if it were credited; recall was too low.
            with_savings(result("c", EvalMode::Symbols, 0.0, false, 1, 100), 100_000, false, 3.0),
        ];
        let rep = aggregate(&rs, "o200k", "/r", "t", 0.8);
        let sv = rep.modes[&EvalMode::Symbols].savings.as_ref().unwrap();
        assert_eq!((sv.credited, sv.uncredited), (2, 1));
        // ratios 10x and 5x -> nearest-rank median = 5x, p90 = 10x
        assert_eq!(sv.ratio.median, 5.0);
        assert_eq!(sv.ratio.p90, 10.0);
        assert_eq!(sv.baseline_tokens_total, 1500);
        assert_eq!(sv.model_ratio, Some(3.0));
        assert!((sv.deviation.unwrap() - (2.0 / 3.0)).abs() < 1e-9);
        assert!(sv.flagged);

        let close = vec![with_savings(
            result("a", EvalMode::Symbols, 1.0, false, 1, 100),
            320,
            true,
            3.0,
        )];
        let sv = aggregate(&close, "o", "/", "t", 0.8).modes[&EvalMode::Symbols]
            .savings
            .clone()
            .unwrap();
        assert!(!sv.flagged, "3.2x vs 3x is within 25%");
    }

    #[test]
    fn compare_flags_quality_latency_token_and_missing_mode_regressions() {
        let base = aggregate(
            &[
                result("a", EvalMode::Symbols, 1.0, false, 10_000, 100),
                result("b", EvalMode::Find, 1.0, true, 10_000, 100),
            ],
            "o",
            "/",
            "t",
            0.8,
        );
        let same = compare(&base, &base, 0.02, 0.25);
        assert!(same.is_empty(), "{same:?}");

        let cur = aggregate(
            &[result("a", EvalMode::Symbols, 0.5, false, 20_000, 200)],
            "o",
            "/",
            "t",
            0.8,
        );
        let regs = compare(&base, &cur, 0.02, 0.25);
        let has = |mode: &str, metric: &str| regs.iter().any(|r| r.mode == mode && r.metric == metric);
        assert!(has("symbols", "mean_f1"));
        assert!(has("symbols", "latency_p95_us"));
        assert!(has("symbols", "mean_tokens"));
        assert!(has("find", "tasks"));

        let within = aggregate(
            &[result("a", EvalMode::Symbols, 0.99, false, 10_500, 110)],
            "o",
            "/",
            "t",
            0.8,
        );
        let only_find = compare(&base, &within, 0.02, 0.25);
        assert!(only_find.iter().all(|r| r.mode == "find"), "{only_find:?}");
    }

    #[test]
    fn markdown_lists_each_mode_and_flags() {
        let rs = vec![with_savings(
            result("a", EvalMode::Symbols, 1.0, false, 1, 100),
            1000,
            true,
            3.0,
        )];
        let md = markdown(&aggregate(&rs, "o200k", "/r", "t", 0.8));
        assert!(md.contains("| symbols | 1 |"));
        assert!(md.contains("DEVIATES"));
    }
}
