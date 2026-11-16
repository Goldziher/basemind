//! The ADR-0022 value gate, evaluated per task kind on one corpus (one report).
//!
//! A task is credited when the tool's recall reaches `--min-recall` (0.8 in the gate). A kind
//! passes when at least [`GATE_MIN_CREDITED`] of its tasks are credited and the median of
//! baseline tokens / tool tokens over the credited tasks is at least [`GATE_MIN_RATIO`]. Tokens
//! counted as `bytes/4` do not count, so without the `tokenizer` feature there is no verdict.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::report::{Report, TaskResult, percentile};

/// Least median baseline/tool token ratio over the credited tasks (ADR-0022 "Pass").
pub const GATE_MIN_RATIO: f64 = 1.5;
/// Least fraction of a kind's tasks that must be credited (ADR-0022 "Pass").
pub const GATE_MIN_CREDITED: f64 = 0.7;
/// The only tokenizer whose counts the gate accepts.
pub const GATE_TOKENIZER: &str = "o200k";

/// One task kind's verdict.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct GateVerdict {
    /// Every task of the kind, errors and baseline failures included.
    pub tasks: usize,
    pub credited: usize,
    pub credited_fraction: f64,
    /// Nearest-rank median of baseline/tool tokens over the credited tasks; absent with none.
    pub median_ratio: Option<f64>,
    /// `None` when the gate cannot be evaluated (`reason` says why).
    pub pass: Option<bool>,
    pub reason: String,
}

/// The verdict for one kind's results counted with `tokenizer`.
pub fn verdict(results: &[&TaskResult], tokenizer: &str) -> GateVerdict {
    let tasks = results.len();
    let mut ratios: Vec<f64> = results
        .iter()
        .filter_map(|r| r.savings.as_ref())
        .filter(|s| s.credited)
        .filter_map(|s| s.ratio)
        .collect();
    let credited = results
        .iter()
        .filter(|r| r.savings.as_ref().is_some_and(|s| s.credited))
        .count();
    ratios.sort_by(f64::total_cmp);
    let median_ratio = (!ratios.is_empty()).then(|| percentile(&ratios, 50.0));
    let credited_fraction = if tasks == 0 {
        0.0
    } else {
        credited as f64 / tasks as f64
    };
    let (pass, reason) = if tokenizer != GATE_TOKENIZER {
        (None, format!("tokens counted as `{tokenizer}`, not `{GATE_TOKENIZER}`"))
    } else if tasks == 0 {
        (None, "no tasks".to_string())
    } else {
        let mut why = Vec::new();
        if credited_fraction < GATE_MIN_CREDITED {
            why.push(format!("credited {credited_fraction:.2} < {GATE_MIN_CREDITED}"));
        }
        match median_ratio {
            Some(m) if m < GATE_MIN_RATIO => why.push(format!("median {m:.2}x < {GATE_MIN_RATIO}x")),
            None => why.push("no credited task".to_string()),
            Some(_) => {}
        }
        (Some(why.is_empty()), why.join("; "))
    };
    GateVerdict {
        tasks,
        credited,
        credited_fraction,
        median_ratio,
        pass,
        reason,
    }
}

/// The gate table, one row per task kind, with the baseline's own recall beside the tool's.
pub fn gate_markdown(r: &Report) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "\n## Value gate (ADR-0022: credited >= {GATE_MIN_CREDITED}, median >= {GATE_MIN_RATIO}x)\n"
    );
    let _ = writeln!(
        s,
        "| kind | tool | tasks | credited | median x | tool R | baseline R | baseline R>=min | symbol acc | verdict |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|");
    for (mode, v) in &r.gate {
        let m = r.modes.get(mode);
        let verdict = match v.pass {
            Some(true) => "PASS".to_string(),
            Some(false) => format!("FAIL ({})", v.reason),
            None => format!("n/a ({})", v.reason),
        };
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} ({:.2}) | {} | {:.3} | {} | {}/{} | {} | {verdict} |",
            mode.as_str(),
            mode.tool_key(),
            v.tasks,
            v.credited,
            v.credited_fraction,
            v.median_ratio.map_or_else(|| "-".into(), |x| format!("{x:.2}")),
            m.map_or(0.0, |m| m.mean_recall),
            m.and_then(|m| m.baseline_mean_recall)
                .map_or_else(|| "-".into(), |x| format!("{x:.3}")),
            m.map_or(0, |m| m.baseline_recall_met),
            m.map_or(0, |m| m.baseline_scored),
            m.and_then(|m| m.mean_annotation_accuracy)
                .map_or_else(|| "-".into(), |x| format!("{x:.3}")),
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::report::SavingsOutcome;
    use crate::eval::score::Score;
    use crate::eval::task::EvalMode;

    fn task(tokens: u64, baseline: u64, credited: bool) -> TaskResult {
        TaskResult {
            id: "t".into(),
            mode: EvalMode::Symbols,
            ok: true,
            error: None,
            elapsed_us: 1,
            tokens,
            pages: 1,
            pages_cut: false,
            annotation_accuracy: None,
            score: Score::default(),
            ranked: false,
            returned: vec![],
            savings: Some(SavingsOutcome {
                baseline_tokens: baseline,
                ratio: Some(baseline as f64 / tokens as f64),
                credited,
                ..SavingsOutcome::default()
            }),
            baseline_score: None,
            baseline_error: None,
        }
    }

    fn run(v: &[TaskResult], tokenizer: &str) -> GateVerdict {
        verdict(&v.iter().collect::<Vec<_>>(), tokenizer)
    }

    #[test]
    fn passes_at_both_thresholds_exactly() {
        // 7 of 10 credited, credited ratios 1.5 .. 2.0: median (nearest-rank) 1.5.
        let mut v: Vec<TaskResult> = [150u64, 150, 150, 150, 200, 200, 200]
            .iter()
            .map(|b| task(100, *b, true))
            .collect();
        for _ in 0..3 {
            v.push(task(100, 10_000, false));
        }
        let g = run(&v, "o200k");
        assert_eq!((g.tasks, g.credited), (10, 7));
        assert_eq!(g.median_ratio, Some(1.5));
        assert_eq!(g.pass, Some(true), "{}", g.reason);
    }

    #[test]
    fn fails_on_low_credit_or_low_ratio_and_ignores_uncredited_ratios() {
        let mut low_credit: Vec<TaskResult> = (0..6).map(|_| task(100, 1000, true)).collect();
        low_credit.extend((0..4).map(|_| task(100, 1000, false)));
        let g = run(&low_credit, "o200k");
        assert_eq!(g.pass, Some(false));
        assert!(g.reason.contains("credited 0.60"), "{}", g.reason);

        // Uncredited tasks have huge ratios; they must not lift the median.
        let mut cheap: Vec<TaskResult> = (0..8).map(|_| task(100, 120, true)).collect();
        cheap.extend((0..2).map(|_| task(1, 100_000, false)));
        let g = run(&cheap, "o200k");
        assert_eq!(g.median_ratio, Some(1.2));
        assert_eq!(g.pass, Some(false));
        assert!(g.reason.contains("median 1.20x"), "{}", g.reason);
    }

    #[test]
    fn has_no_verdict_without_the_o200k_tokenizer_or_tasks() {
        let v: Vec<TaskResult> = (0..10).map(|_| task(100, 1000, true)).collect();
        let g = run(&v, "bytes/4");
        assert_eq!(g.pass, None);
        assert!(g.reason.contains("bytes/4"));
        assert_eq!(run(&[], "o200k").pass, None);
    }

    #[test]
    fn errors_count_against_the_credited_fraction() {
        let mut v: Vec<TaskResult> = (0..7).map(|_| task(100, 1000, true)).collect();
        for _ in 0..3 {
            let mut e = task(100, 0, false);
            e.ok = false;
            e.savings = None;
            v.push(e);
        }
        let g = run(&v, "o200k");
        assert_eq!((g.tasks, g.credited), (10, 7));
        assert_eq!(g.pass, Some(true));
        v.push({
            let mut e = task(100, 0, false);
            e.savings = None;
            e
        });
        // 7 of 11 is 0.64.
        assert_eq!(run(&v, "o200k").pass, Some(false));
    }
}
