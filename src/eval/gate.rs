//! The value gate of ADR-0022 as ADR-0031 amends it, evaluated per task kind on one corpus (one
//! report).
//!
//! A task is credited when the tool's recall reaches `--min-recall` (0.8 in the gate). Every kind
//! has a [`Job`] ([`job`] is the one table), and the job picks the rule:
//!
//! - **reducing** (the answer replaces reading content): credited at least [`GATE_MIN_CREDITED`]
//!   and median baseline/tool token ratio at least [`REDUCING_MIN_RATIO`] ([`READ_AROUND_MIN_RATIO`]
//!   for `read_around`, ADR-0055);
//! - **locating** (the answer is a list of places): credited at least [`GATE_MIN_CREDITED`], median
//!   ratio at least [`LOCATING_MIN_RATIO`], and the tool's mean recall no more than
//!   [`LOCATING_MAX_RECALL_DEFICIT`] below the baseline's;
//! - **correctness** (no shell command an agent would run produces the answer, so there is no
//!   baseline to compare): credited at least [`GATE_MIN_CREDITED`].
//!
//! `dependents` also needs a mean gold recall of [`DEPENDENTS_MIN_RECALL`], independent of the
//! baseline, since the tool and the baseline can miss the same files.
//!
//! `grep` also needs the enclosing-symbol accuracy of `code_grep` at [`GREP_MIN_SYMBOL_ACCURACY`]
//! (ADR-0031 rule 4).
//!
//! The median is nearest-rank, over the credited tasks whose baseline answered. A baseline that
//! errored or printed nothing is a baseline failure: it is left out of the ratio (an empty shell
//! answer is not a cheap one) and scores recall 0 in the recall comparison. Recall means are taken
//! over the tasks whose baseline was scored, the same tasks on both arms. Tokens counted as
//! `bytes/4` do not count, so without the `tokenizer` feature there is no verdict.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::report::{Report, TaskResult, percentile};
use super::task::EvalMode;

/// Least fraction of a kind's tasks that must be credited, for every job (ADR-0022 "Pass").
pub const GATE_MIN_CREDITED: f64 = 0.7;
/// Least median baseline/tool token ratio of a reducing kind (ADR-0031 rule 2).
pub const REDUCING_MIN_RATIO: f64 = 1.5;
/// Least median baseline/tool token ratio of `read_around` (ADR-0055). Its rows keep their
/// `## <symbol>` headers (ADR-0052, ADR-0053), which the shell's `rg -C` output has no counterpart for,
/// so the kind measured 1.43x on basemind against the 1.5x of the other reducing kinds.
pub const READ_AROUND_MIN_RATIO: f64 = 1.4;
/// Least median baseline/tool token ratio of a locating kind (ADR-0031 rule 3: the tool's answer
/// costs at most one and a half times the shell's), exactly 2/3. A ratio that is 2/3 on paper
/// (`8 / 12`) is the same correctly rounded double, and `EPS` absorbs any other rounding.
pub const LOCATING_MIN_RATIO: f64 = 2.0 / 3.0;
/// Most a locating kind's mean recall may fall below the baseline's (ADR-0031 rule 3).
pub const LOCATING_MAX_RECALL_DEFICIT: f64 = 0.02;
/// Least mean gold recall of `dependents`, whatever the baseline scores. Recall parity cannot fail
/// on a file both arms miss (the gold is syn-flattened grouped `use` paths, which `git grep` and the
/// tool both match lexically), so this floor is the check that can. Conservative placeholder equal
/// to the credit threshold (`--min-recall 0.8`); the owner sets the value.
pub const DEPENDENTS_MIN_RECALL: f64 = 0.8;
/// Least share of `code_grep` hits whose enclosing symbol is right (ADR-0031 rule 4).
pub const GREP_MIN_SYMBOL_ACCURACY: f64 = 0.95;
/// The only tokenizer whose counts the gate accepts.
pub const GATE_TOKENIZER: &str = "o200k";
/// Float slack on every threshold, so a value that equals a threshold on paper (`1.00 - 0.98`)
/// is not failed by rounding.
const EPS: f64 = 1e-9;

/// What a task kind's answer is for, which decides its pass rule (ADR-0031 rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Job {
    /// The answer is a list of places; the shell's answer is already minimal.
    Locating,
    /// The answer replaces reading content. The default, because a report written before jobs
    /// existed was judged by this rule (ADR-0022).
    #[default]
    Reducing,
    /// No shell command an agent would run produces the answer, so only the tool's recall is
    /// judged.
    Correctness,
}

impl Job {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Locating => "locating",
            Self::Reducing => "reducing",
            Self::Correctness => "correctness",
        }
    }
}

/// The job of every task kind: ADR-0031 rule 1 for the kinds it lists, ADR-0051 for the 0.34
/// analysis kinds (`impact` .. `explain`).
pub fn job(mode: EvalMode) -> Job {
    match mode {
        EvalMode::Symbols
        | EvalMode::Grep
        | EvalMode::References
        | EvalMode::Callers
        | EvalMode::Implementations
        | EvalMode::Dependents
        | EvalMode::Find
        | EvalMode::GitSearch
        | EvalMode::GitPath
        | EvalMode::GitSymbol
        | EvalMode::Diff
        | EvalMode::Impact
        | EvalMode::GrepChanged
        | EvalMode::FindLarge => Job::Locating,
        EvalMode::Outline
        | EvalMode::Read
        | EvalMode::Blame
        | EvalMode::BlameSymbol
        | EvalMode::DiffStructural
        | EvalMode::Docs
        | EvalMode::ReadAround
        | EvalMode::Explain => Job::Reducing,
        EvalMode::Cycles => Job::Correctness,
    }
}

/// The median ratio floor of a reducing kind: [`READ_AROUND_MIN_RATIO`] for `read_around`, else
/// [`REDUCING_MIN_RATIO`].
fn reducing_min_ratio(mode: EvalMode) -> f64 {
    if mode == EvalMode::ReadAround {
        READ_AROUND_MIN_RATIO
    } else {
        REDUCING_MIN_RATIO
    }
}

/// One task kind's verdict.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct GateVerdict {
    /// The tool the kind drives (`EvalMode::tool_key`); a failing kind fails it.
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub job: Job,
    /// Every task of the kind, errors and baseline failures included.
    pub tasks: usize,
    pub credited: usize,
    pub credited_fraction: f64,
    /// Tasks whose baseline errored or printed nothing.
    #[serde(default)]
    pub baseline_failed: usize,
    /// Nearest-rank median of baseline/tool tokens over the credited tasks whose baseline
    /// answered; absent with none.
    pub median_ratio: Option<f64>,
    /// Mean recall of the tool over the tasks whose baseline was scored; absent when none was.
    #[serde(default)]
    pub tool_recall: Option<f64>,
    /// Mean recall of the baseline over the same tasks, a failed baseline scoring 0.
    #[serde(default)]
    pub baseline_recall: Option<f64>,
    /// `grep` only: mean share of hits whose enclosing symbol was right.
    #[serde(default)]
    pub symbol_accuracy: Option<f64>,
    /// `None` when the gate cannot be evaluated (`reason` says why).
    pub pass: Option<bool>,
    pub reason: String,
}

fn mean(v: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = v.fold((0.0, 0usize), |(s, n), x| (s + x, n + 1));
    (n > 0).then(|| sum / n as f64)
}

/// The verdict for `mode`'s results counted with `tokenizer`.
pub fn verdict(mode: EvalMode, results: &[&TaskResult], tokenizer: &str) -> GateVerdict {
    let job = job(mode);
    let tasks = results.len();
    let credited = results.iter().filter(|r| r.credited).count();
    let mut ratios: Vec<f64> = results.iter().filter_map(|r| r.gated_ratio()).collect();
    ratios.sort_by(f64::total_cmp);
    let median_ratio = (!ratios.is_empty()).then(|| percentile(&ratios, 50.0));
    let credited_fraction = if tasks == 0 {
        0.0
    } else {
        credited as f64 / tasks as f64
    };
    let paired: Vec<(f64, f64)> = results
        .iter()
        .filter_map(|r| r.baseline_score.as_ref().map(|b| (r.score.recall, b.recall)))
        .collect();
    let tool_recall = mean(paired.iter().map(|p| p.0));
    let baseline_recall = mean(paired.iter().map(|p| p.1));
    let symbol_accuracy = mean(results.iter().filter_map(|r| r.annotation_accuracy));
    let (pass, reason) = if tokenizer != GATE_TOKENIZER {
        (None, format!("tokens counted as `{tokenizer}`, not `{GATE_TOKENIZER}`"))
    } else if tasks == 0 {
        (None, "no tasks".to_string())
    } else {
        let mut checks = Checks::default();
        checks.credited(credited_fraction);
        match job {
            Job::Reducing => checks.ratio(credited, median_ratio, reducing_min_ratio(mode)),
            Job::Locating => {
                checks.ratio(credited, median_ratio, LOCATING_MIN_RATIO);
                checks.recall_parity(tool_recall, baseline_recall);
            }
            Job::Correctness => {}
        }
        if mode == EvalMode::Dependents {
            checks.gold_recall(mean(results.iter().map(|r| r.score.recall)), DEPENDENTS_MIN_RECALL);
        }
        if mode == EvalMode::Grep {
            checks.symbol_accuracy(symbol_accuracy);
        }
        checks.outcome()
    };
    GateVerdict {
        tool: mode.tool_key().to_string(),
        job,
        tasks,
        credited,
        credited_fraction,
        baseline_failed: results.iter().filter(|r| r.baseline_failed).count(),
        median_ratio,
        tool_recall,
        baseline_recall,
        symbol_accuracy,
        pass,
        reason,
    }
}

/// The failed and the undecidable checks of one verdict. A failed check fails the kind; otherwise
/// an undecidable one leaves it without a verdict.
#[derive(Default)]
struct Checks {
    failed: Vec<String>,
    undecided: Vec<String>,
}

impl Checks {
    fn credited(&mut self, fraction: f64) {
        if fraction < GATE_MIN_CREDITED - EPS {
            self.failed
                .push(format!("credited {fraction:.2} < {GATE_MIN_CREDITED}"));
        }
    }

    fn ratio(&mut self, credited: usize, median: Option<f64>, min: f64) {
        match median {
            Some(m) if m < min - EPS => {
                let (m, min) = distinct(m, min);
                self.failed.push(format!("median {m}x < {min}x"));
            }
            None if credited > 0 => self
                .undecided
                .push("no credited task has a baseline answer to compare".to_string()),
            _ => {}
        }
    }

    fn recall_parity(&mut self, tool: Option<f64>, baseline: Option<f64>) {
        match (tool, baseline) {
            (Some(t), Some(b)) if b - t > LOCATING_MAX_RECALL_DEFICIT + EPS => self.failed.push(format!(
                "recall {t:.3} < baseline {b:.3} - {LOCATING_MAX_RECALL_DEFICIT}"
            )),
            (Some(_), Some(_)) => {}
            _ => self
                .undecided
                .push("no baseline recall to compare (no baseline names an `items` reader)".to_string()),
        }
    }

    fn gold_recall(&mut self, recall: Option<f64>, min: f64) {
        if let Some(r) = recall.filter(|r| *r < min - EPS) {
            self.failed.push(format!("gold recall {r:.3} < {min:.3}"));
        }
    }

    fn symbol_accuracy(&mut self, accuracy: Option<f64>) {
        match accuracy {
            Some(a) if a < GREP_MIN_SYMBOL_ACCURACY - EPS => self
                .failed
                .push(format!("symbol accuracy {a:.3} < {GREP_MIN_SYMBOL_ACCURACY}")),
            Some(_) => {}
            None => self
                .undecided
                .push("no symbol accuracy measured (no task carries `annotations`)".to_string()),
        }
    }

    fn outcome(self) -> (Option<bool>, String) {
        if !self.failed.is_empty() {
            (Some(false), self.failed.join("; "))
        } else if !self.undecided.is_empty() {
            (None, self.undecided.join("; "))
        } else {
            (Some(true), String::new())
        }
    }
}

/// `value` and `threshold` printed with the fewest digits (at least 2) that tell them apart, so a
/// failed check never reads as one that cannot fail (`0.67x < 0.67x` for 0.6662 against 2/3).
fn distinct(value: f64, threshold: f64) -> (String, String) {
    let shown = |digits: usize| (format!("{value:.digits$}"), format!("{threshold:.digits$}"));
    (2..=17).map(shown).find(|(v, t)| v != t).unwrap_or_else(|| shown(17))
}

fn opt(v: Option<f64>, digits: usize) -> String {
    v.map_or_else(|| "-".into(), |x| format!("{x:.digits$}"))
}

/// The gate table, one row per task kind, with the baseline's own recall beside the tool's.
pub fn gate_markdown(r: &Report) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "\n## Value gate (ADR-0031: credited >= {GATE_MIN_CREDITED} for every job; reducing median >= \
         {REDUCING_MIN_RATIO}x ({READ_AROUND_MIN_RATIO}x for read_around, ADR-0055); locating median >= {LOCATING_MIN_RATIO:.4}x (2/3) and recall >= baseline - \
         {LOCATING_MAX_RECALL_DEFICIT}; grep symbol accuracy >= {GREP_MIN_SYMBOL_ACCURACY})\n"
    );
    let _ = writeln!(
        s,
        "| kind | tool | job | tasks | credited | baseline failed | median x | tool R | baseline R | symbol acc | verdict |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|---|");
    for (mode, v) in &r.gate {
        let verdict = match v.pass {
            Some(true) => "PASS".to_string(),
            Some(false) => format!("FAIL ({})", v.reason),
            None => format!("n/a ({})", v.reason),
        };
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} ({:.2}) | {} | {} | {} | {} | {} | {verdict} |",
            mode.as_str(),
            mode.tool_key(),
            v.job.as_str(),
            v.tasks,
            v.credited,
            v.credited_fraction,
            v.baseline_failed,
            opt(v.median_ratio, 2),
            opt(v.tool_recall, 3),
            opt(v.baseline_recall, 3),
            opt(v.symbol_accuracy, 3),
        );
    }
    s
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
