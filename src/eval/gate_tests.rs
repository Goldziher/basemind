use super::*;
use crate::eval::report::SavingsOutcome;
use crate::eval::score::Score;

/// A task the tool answered in `tokens` with recall 1 (credited) or 0, against a baseline of
/// `baseline` tokens; no baseline recall.
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
        score: Score {
            recall: if credited { 1.0 } else { 0.0 },
            ..Score::default()
        },
        credited,
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
        baseline_failed: false,
    }
}

/// `task` with both arms' recall set, as a baseline with an `items` reader leaves it.
fn scored(tokens: u64, baseline: u64, tool_recall: f64, baseline_recall: f64) -> TaskResult {
    let mut t = task(tokens, baseline, tool_recall >= 0.8);
    t.score.recall = tool_recall;
    t.baseline_score = Some(Score {
        recall: baseline_recall,
        ..Score::default()
    });
    t
}

fn run(mode: EvalMode, v: &[TaskResult]) -> GateVerdict {
    verdict(mode, &v.iter().collect::<Vec<_>>(), "o200k")
}

/// 7 credited tasks at the given ratios (in percent of the tool's 100 tokens), then 3 uncredited.
fn seven_of_ten(baselines: [u64; 7]) -> Vec<TaskResult> {
    let mut v: Vec<TaskResult> = baselines.iter().map(|b| scored(100, *b, 1.0, 1.0)).collect();
    v.extend((0..3).map(|_| scored(100, 10_000, 0.0, 0.0)));
    v
}

#[test]
fn the_job_table_follows_adr_0031_and_classifies_the_analysis_kinds() {
    use EvalMode::*;
    for m in [
        Symbols,
        Grep,
        References,
        Callers,
        Implementations,
        Dependents,
        Find,
        GitSearch,
        GitPath,
        GitSymbol,
        Diff,
        Impact,
        GrepChanged,
        FindLarge,
    ] {
        assert_eq!(job(m), Job::Locating, "{}", m.as_str());
    }
    for m in [
        Outline,
        Read,
        Blame,
        BlameSymbol,
        DiffStructural,
        Docs,
        ReadAround,
        Explain,
    ] {
        assert_eq!(job(m), Job::Reducing, "{}", m.as_str());
    }
    assert_eq!(job(Cycles), Job::Correctness);
    let g = run(FindLarge, &[scored(100, 100, 1.0, 1.0)]);
    assert_eq!((g.tool.as_str(), g.job), ("find_files", Job::Locating));
}

#[test]
fn reducing_passes_at_credited_0_7_and_median_1_5_and_fails_just_below_either() {
    let g = run(EvalMode::Read, &seven_of_ten([150, 150, 150, 150, 200, 200, 200]));
    assert_eq!((g.tasks, g.credited, g.median_ratio), (10, 7, Some(1.5)));
    assert_eq!(g.pass, Some(true), "{}", g.reason);

    let g = run(EvalMode::Read, &seven_of_ten([149, 149, 149, 149, 200, 200, 200]));
    assert_eq!(g.pass, Some(false));
    assert!(g.reason.contains("median 1.49x < 1.50x"), "{}", g.reason);

    let mut low = seven_of_ten([1000; 7]);
    low[6] = scored(100, 1000, 0.0, 1.0);
    let g = run(EvalMode::Read, &low);
    assert_eq!(g.pass, Some(false));
    assert!(g.reason.contains("credited 0.60 < 0.7"), "{}", g.reason);
}

#[test]
fn locating_passes_at_median_0_67_and_fails_at_0_66_where_reducing_fails_both() {
    let at = seven_of_ten([67, 67, 67, 67, 90, 90, 90]);
    let g = run(EvalMode::Symbols, &at);
    assert_eq!(g.median_ratio, Some(0.67));
    assert_eq!(g.pass, Some(true), "{}", g.reason);
    assert_eq!(
        run(EvalMode::Outline, &at).pass,
        Some(false),
        "a reducing kind needs 1.5x"
    );

    let g = run(EvalMode::Symbols, &seven_of_ten([66, 66, 66, 66, 90, 90, 90]));
    assert_eq!(g.pass, Some(false));
    assert!(g.reason.contains("median 0.66x < 0.67x"), "{}", g.reason);
}

#[test]
fn locating_passes_at_exactly_one_and_a_half_times_the_shells_cost_and_fails_just_over() {
    // ADR-0031 rule 3: "at most one and a half times the shell's". 12 tokens against 8 and 15
    // against 10 are exactly 1.5x, the median `find` sat on in the third 0.34 gate run.
    for (tool, shell) in [(12, 8), (15, 10), (150, 100)] {
        let v: Vec<TaskResult> = (0..10).map(|_| scored(tool, shell, 1.0, 1.0)).collect();
        let g = run(EvalMode::Find, &v);
        assert_eq!(g.pass, Some(true), "{tool} vs {shell}: {}", g.reason);
    }

    let over: Vec<TaskResult> = (0..10).map(|_| scored(1501, 1000, 1.0, 1.0)).collect();
    let g = run(EvalMode::Find, &over);
    assert_eq!(g.pass, Some(false), "1501 tokens against 1000 is over 1.5x");
    assert!(g.reason.contains("median 0.666x < 0.667x"), "{}", g.reason);

    let barely: Vec<TaskResult> = (0..10).map(|_| scored(150_001, 100_000, 1.0, 1.0)).collect();
    let g = run(EvalMode::Find, &barely);
    assert_eq!(g.pass, Some(false));
    let (shown, floor) = g
        .reason
        .strip_prefix("median ")
        .and_then(|r| r.split_once("x < "))
        .expect("a median reason");
    assert_ne!(
        shown,
        floor.trim_end_matches('x'),
        "a failure never prints equal numbers: {}",
        g.reason
    );
}

#[test]
fn locating_recall_may_trail_the_baseline_by_0_02_and_no_more() {
    let within: Vec<TaskResult> = (0..10).map(|_| scored(100, 100, 0.98, 1.0)).collect();
    let g = run(EvalMode::References, &within);
    assert!((g.tool_recall.unwrap() - 0.98).abs() < 1e-9, "{g:?}");
    assert_eq!(g.baseline_recall, Some(1.0));
    assert_eq!(g.pass, Some(true), "{}", g.reason);

    let beyond: Vec<TaskResult> = (0..10).map(|_| scored(100, 200, 0.97, 1.0)).collect();
    let g = run(EvalMode::References, &beyond);
    assert_eq!(g.pass, Some(false));
    assert!(
        g.reason.contains("recall 0.970 < baseline 1.000 - 0.02"),
        "{}",
        g.reason
    );
    assert_eq!(
        run(EvalMode::Blame, &beyond).pass,
        Some(true),
        "recall parity is a locating rule only"
    );

    let unscored: Vec<TaskResult> = (0..10).map(|_| task(100, 100, true)).collect();
    let g = run(EvalMode::References, &unscored);
    assert_eq!(g.pass, None, "a locating kind without baseline recall cannot pass");
    assert!(g.reason.contains("no baseline recall"), "{}", g.reason);
}

#[test]
fn grep_needs_symbol_accuracy_0_95() {
    let with = |acc: Option<f64>| -> Vec<TaskResult> {
        (0..10)
            .map(|_| {
                let mut t = scored(100, 100, 1.0, 1.0);
                t.annotation_accuracy = acc;
                t
            })
            .collect()
    };
    assert_eq!(run(EvalMode::Grep, &with(Some(0.95))).pass, Some(true));
    let g = run(EvalMode::Grep, &with(Some(0.94)));
    assert_eq!(g.pass, Some(false));
    assert!(g.reason.contains("symbol accuracy 0.940 < 0.95"), "{}", g.reason);
    let g = run(EvalMode::Grep, &with(None));
    assert_eq!(g.pass, None, "{}", g.reason);
    assert_eq!(
        run(EvalMode::GrepChanged, &with(None)).pass,
        Some(true),
        "only `grep` carries the enclosing symbol"
    );
}

#[test]
fn a_correctness_kind_is_judged_on_credited_alone() {
    // No baseline at all: `credited` comes from the tool's recall alone, and there is no ratio.
    let v: Vec<TaskResult> = (0..10)
        .map(|i| {
            let mut t = task(100, 0, i < 7);
            t.savings = None;
            t
        })
        .collect();
    let g = run(EvalMode::Cycles, &v);
    assert_eq!((g.credited, g.median_ratio), (7, None));
    assert_eq!(g.pass, Some(true), "{}", g.reason);
    let mut fewer = v;
    fewer[0].credited = false;
    assert_eq!(run(EvalMode::Cycles, &fewer).pass, Some(false));
}

#[test]
fn a_failed_baseline_scores_recall_0_and_stays_out_of_the_ratio() {
    // Five baselines answered at 1x; five printed nothing (0 tokens) or errored. Counted in the
    // ratio, the empty ones would put the median at 0x.
    let mut v: Vec<TaskResult> = (0..5).map(|_| scored(100, 100, 1.0, 1.0)).collect();
    v.extend((0..5).map(|_| {
        let mut t = scored(100, 0, 1.0, 0.0);
        t.baseline_failed = true;
        t
    }));
    let g = run(EvalMode::Symbols, &v);
    assert_eq!((g.credited, g.baseline_failed), (10, 5));
    assert_eq!(g.median_ratio, Some(1.0));
    assert_eq!(g.baseline_recall, Some(0.5), "a failed baseline counts as recall 0");
    assert_eq!(g.pass, Some(true), "{}", g.reason);

    for t in &mut v {
        t.baseline_failed = true;
    }
    let g = run(EvalMode::Symbols, &v);
    assert_eq!(g.median_ratio, None);
    assert_eq!(g.pass, None, "no baseline answered, so there is no ratio: {}", g.reason);
}

#[test]
fn has_no_verdict_without_the_o200k_tokenizer_or_tasks() {
    let v: Vec<TaskResult> = (0..10).map(|_| scored(100, 1000, 1.0, 1.0)).collect();
    let g = verdict(EvalMode::Read, &v.iter().collect::<Vec<_>>(), "bytes/4");
    assert_eq!(g.pass, None);
    assert!(g.reason.contains("bytes/4"));
    assert_eq!(run(EvalMode::Read, &[]).pass, None);
}

#[test]
fn errors_count_against_the_credited_fraction() {
    let mut v: Vec<TaskResult> = (0..7).map(|_| scored(100, 1000, 1.0, 1.0)).collect();
    for _ in 0..3 {
        let mut e = task(100, 0, false);
        e.ok = false;
        e.savings = None;
        v.push(e);
    }
    let g = run(EvalMode::Read, &v);
    assert_eq!((g.tasks, g.credited), (10, 7));
    assert_eq!(g.pass, Some(true), "{}", g.reason);
    v.push({
        let mut e = task(100, 0, false);
        e.savings = None;
        e
    });
    // 7 of 11 is 0.64.
    assert_eq!(run(EvalMode::Read, &v).pass, Some(false));
}

#[test]
fn dependents_fail_on_misses_the_baseline_shares() {
    // Tool and baseline both find 5 of 10 gold files: parity holds and the ratio is fine, but the
    // gold recall is under the floor.
    let v: Vec<TaskResult> = (0..10).map(|_| scored(100, 100, 0.5, 0.5)).collect();
    let g = run(EvalMode::Dependents, &v);
    assert_eq!(g.pass, Some(false));
    assert!(g.reason.contains("gold recall 0.500 < 0.800"), "{}", g.reason);
    // The same numbers on another locating kind pass the parity rule, so the floor is what failed.
    assert!(!run(EvalMode::Symbols, &v).reason.contains("gold recall"));
}

#[test]
fn dependents_pass_at_the_gold_recall_floor() {
    let v: Vec<TaskResult> = (0..10)
        .map(|_| scored(100, 100, DEPENDENTS_MIN_RECALL, DEPENDENTS_MIN_RECALL))
        .collect();
    let g = run(EvalMode::Dependents, &v);
    assert_eq!(g.pass, Some(true), "{}", g.reason);
}

#[test]
fn dependents_floor_counts_a_failed_call_as_recall_0() {
    let mut v: Vec<TaskResult> = (0..9).map(|_| scored(100, 100, 1.0, 1.0)).collect();
    let mut e = task(100, 100, false);
    e.ok = false;
    v.push(e);
    let g = run(EvalMode::Dependents, &v);
    assert_eq!(g.pass, Some(true), "9 of 10 at recall 1 is 0.9: {}", g.reason);
    for t in v.iter_mut().take(3) {
        t.score.recall = 0.0;
    }
    assert_eq!(run(EvalMode::Dependents, &v).pass, Some(false));
}
