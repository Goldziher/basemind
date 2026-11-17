//! Heuristic estimator for "how many tokens did this basemind tool call save the agent vs the
//! grep + Read baseline?". Honest about being a heuristic — every row carries the baseline name
//! so the dashboard can disclose the assumption.
//!
//! Token counting has two tiers. When the **full response text** is in hand, the figures route
//! through [`super::tokens::count_tokens`] — a real o200k (gpt-4o) tokenizer under the `tokenizer`
//! feature (pulled in unconditionally by `documents`), a `bytes / 4` heuristic otherwise. When only a **byte length** is available (the live
//! telemetry path, whose caller has already collapsed the response to a byte count), there is no
//! text to tokenize, so it falls back to the same `bytes / 4` rule of thumb basemind's scan-cost
//! reporting uses. Under default features the two tiers are numerically identical.

use serde::Serialize;

/// One row's worth of "tokens saved" reasoning. The `est_tokens_saved` field is what the
/// dashboard sums; the `baseline` field is the disclosed assumption.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SavingsRow {
    /// Estimated tokens the agent would have spent without basemind.
    pub baseline_tokens: u64,
    /// Estimated tokens spent on this call's response.
    pub actual_tokens: u64,
    /// `baseline_tokens - actual_tokens`, saturating at 0.
    pub est_tokens_saved: u64,
    /// Disclosed name of the baseline model — see the table below.
    pub baseline: &'static str,
}

/// Real token count of `text`, routed through [`super::tokens::count_tokens`]: a true o200k
/// (gpt-4o) tokenizer under the `documents` feature, `bytes / 4` otherwise. Use this — not
/// [`super::tokens::bytes_to_tokens`] — wherever the full response text is available, so telemetry
/// reports honest token figures when a tokenizer is compiled in.
fn tokens_for_text(text: &str) -> u64 {
    super::tokens::count_tokens(text)
}

/// Scale a response's token count by a baseline multiplier expressed in percent (`120` = 1.2x).
/// Multipliers are fractional because several lookups save little: the baseline is a plain `rg`
/// output, which is already compact.
fn scaled(actual: u64, pct: u64) -> u64 {
    actual.saturating_mul(pct) / 100
}

// Baseline multipliers, in percent. Each is the LOWEST per-corpus median token ratio (baseline
// tokens / basemind response tokens) the committed value gate measured for the tool over every task
// kind it answers (`benchmarks/eval/results/gate.json`), rounded down to a tenth. One telemetry key
// covers every kind of a tool, so the weakest kind sets the figure. A tool whose measured median is
// below 1, or that the gate did not run, claims 100: no saving. Re-run the gate and update these when
// a response shape changes; `multipliers_never_exceed_the_committed_gate` holds them to it.

/// `code_outline` vs reading the whole file: 3.49x on basemind, 1.69x on tokio.
const OUTLINE_PCT: u64 = 160;

/// `code_read` vs reading the whole file the span is in: 32.7x on basemind, 25.9x on tokio.
const READ_PCT: u64 = 2580;

/// `git_blame` vs `git blame -L`: its weakest kind (a symbol's blame on tokio) measured 1.11x.
const BLAME_PCT: u64 = 110;

/// Tools the gate measured below 1x against their baseline, or did not run: `code_symbols` (0.29x vs
/// `git grep`), `code_refs`, `find_files`, `code_grep`, `git_log`, `git_diff` (1.00x for a file diff),
/// `code_search` and `docs_search`. They keep their disclosed baseline and claim no saving.
const NO_SAVING_PCT: u64 = 100;

/// The multiplier and disclosed baseline of each tool the gate measured.
const MEASURED: &[(&str, u64, &str)] = &[
    ("code_outline", OUTLINE_PCT, "full_file_read"),
    ("code_read", READ_PCT, "full_file_read"),
    ("git_blame", BLAME_PCT, "git_blame"),
    ("code_symbols", NO_SAVING_PCT, "git_grep"),
    ("code_refs", NO_SAVING_PCT, "grep_top_hits"),
    ("find_files", NO_SAVING_PCT, "git_ls_files_grep"),
];

/// Whether a response answered with nothing: a `total` of 0, or every list and map it carries
/// empty. An empty answer saved the agent nothing, whatever the tool's multiplier.
fn returned_nothing(text: &str) -> bool {
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_str::<serde_json::Value>(text) else {
        return text.trim().is_empty();
    };
    if fields.get("total").and_then(serde_json::Value::as_u64) == Some(0) {
        return true;
    }
    let mut collections = fields.values().filter(|v| v.is_array() || v.is_object()).peekable();
    collections.peek().is_some()
        && collections
            .all(|v| v.as_array().is_some_and(Vec::is_empty) || v.as_object().is_some_and(serde_json::Map::is_empty))
}

/// Tools with no measured baseline: they book the response as its own baseline.
const KNOWN_TOOLS: &[&str] = &[
    "code_grep",
    "code_search",
    "docs_search",
    "git_log",
    "git_diff",
    "memory_save",
    "memory_search",
    "index_status",
    "index_rescan",
];

/// Telemetry keys written before ADR-0018, mapped to the tool that replaced them. Rows recorded
/// by an older binary (`code:outline`) and tool names older clients still send (`code_references`)
/// keep resolving to a baseline instead of falling through to `unclassified`.
const LEGACY_KEYS: &[(&str, &str)] = &[
    ("code:outline", "code_outline"),
    ("code:symbols", "code_symbols"),
    ("code:grep", "code_grep"),
    ("code:expand", "code_read"),
    ("code:semantic", "code_search"),
    ("code:references", "code_refs"),
    ("code:callers", "code_refs"),
    ("code:implementations", "code_refs"),
    ("code:dependents", "code_refs"),
    ("code:definition", "code_refs"),
    ("code:find", "find_files"),
    ("code:files", "find_files"),
    ("memory:documents", "docs_search"),
    ("memory:put", "memory_save"),
    ("memory:get", "memory_search"),
    ("memory:list", "memory_search"),
    ("memory:search", "memory_search"),
    ("git:recent", "git_log"),
    ("git:touching", "git_log"),
    ("git:by_path", "git_log"),
    ("git:search", "git_log"),
    ("git:show", "git_log"),
    ("git:symbol_history", "git_log"),
    ("git:blame", "git_blame"),
    ("git:blame_symbol", "git_blame"),
    ("git:diff", "git_diff"),
    ("git:diff_outline", "git_diff"),
    ("git:status", "git_diff"),
    ("admin:status", "index_status"),
    ("admin:repo", "index_status"),
    ("admin:rescan", "index_rescan"),
];

/// The tool name the baseline table is written against, for a current name or a legacy key in
/// either spelling (`code:outline` / `code_outline`, `code:references` / `code_references`).
fn canonical_key(tool: &str) -> &str {
    LEGACY_KEYS
        .iter()
        .find(|(legacy, _)| {
            *legacy == tool
                || legacy.split_once(':').is_some_and(|(domain, mode)| {
                    tool.strip_prefix(domain).and_then(|rest| rest.strip_prefix('_')) == Some(mode)
                })
        })
        .map_or(tool, |(_, current)| current)
}

/// Estimate baseline + actual tokens for one tool call from the full response **text**.
///
/// The live telemetry entry point. The `actual` count routes through [`tokens_for_text`] — a
/// real o200k tokenizer under the `documents` feature, the `bytes / 4` heuristic otherwise —
/// so telemetry reports honest counts when a tokenizer is compiled in. The byte-only fallback
/// ([`super::tokens::bytes_to_tokens`]) remains for paths that hold only a byte length, e.g. the
/// budget loop.
///
/// `corpus_bytes` is the total byte count of every indexed file (held on `ServerState` and
/// recomputed after each rescan). Retained for signature stability and potential future
/// per-tool models; the grep-style baselines are now corpus-independent (derived from the
/// response payload), so this argument currently goes unused.
pub fn estimate_from_text(tool: &str, _corpus_bytes: u64, resp_text: &str) -> SavingsRow {
    let actual = tokens_for_text(resp_text);
    let key = canonical_key(tool);
    let (baseline, baseline_name) = match MEASURED.iter().find(|(name, _, _)| *name == key) {
        Some((_, pct, name)) if *pct > NO_SAVING_PCT && !returned_nothing(resp_text) => (scaled(actual, *pct), *name),
        Some((_, _, name)) => (actual, *name),
        None if KNOWN_TOOLS.contains(&key) => (actual, "no_baseline"),
        None => (actual, "unclassified"),
    };

    SavingsRow {
        baseline_tokens: baseline,
        actual_tokens: actual,
        est_tokens_saved: baseline.saturating_sub(actual),
        baseline: baseline_name,
    }
}

/// The row for an `isError` result: the agent got a refusal to act on, not an answer, so nothing
/// was saved.
pub fn refusal(resp_text: &str) -> SavingsRow {
    let actual = tokens_for_text(resp_text);
    SavingsRow {
        baseline_tokens: actual,
        actual_tokens: actual,
        est_tokens_saved: 0,
        baseline: "tool_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = "{\"hits\":[{\"path\":\"src/a.rs\",\"line\":3}],\"total\":1}";

    fn row(tool: &str, text: &str) -> SavingsRow {
        estimate_from_text(tool, 1_000_000, text)
    }

    #[test]
    fn outline_and_read_scale_the_response_by_their_measured_median() {
        for (tool, pct) in [
            ("code_outline", OUTLINE_PCT),
            ("code_read", READ_PCT),
            ("git_blame", BLAME_PCT),
        ] {
            let s = row(tool, ANSWER);
            assert_eq!(s.baseline_tokens, scaled(s.actual_tokens, pct), "{tool}");
            assert_eq!(s.est_tokens_saved, s.baseline_tokens - s.actual_tokens, "{tool}");
            assert!(s.est_tokens_saved > 0, "{tool}");
        }
    }

    /// The gate measured `code_symbols` at 0.29x-0.32x against `git grep`: a lookup costs more than
    /// the grep it replaces, so it books nothing, whatever it returned.
    #[test]
    fn tools_that_did_not_beat_their_baseline_claim_no_saving() {
        for (tool, baseline) in [
            ("code_symbols", "git_grep"),
            ("code_refs", "grep_top_hits"),
            ("find_files", "git_ls_files_grep"),
        ] {
            let s = row(tool, ANSWER);
            assert_eq!(s.baseline, baseline, "{tool} baseline label");
            assert_eq!(s.baseline_tokens, s.actual_tokens, "{tool} must model a 1x baseline");
            assert_eq!(s.est_tokens_saved, 0, "{tool} must not claim savings");
        }
    }

    #[test]
    fn an_empty_answer_saves_nothing() {
        for text in [
            "{\"hits\":[],\"total\":0}",
            "{\"symbols\":[],\"imports\":[]}",
            "{\"hits\":{},\"next_cursor\":null}",
            "",
        ] {
            for tool in ["code_outline", "code_read", "git_blame"] {
                assert_eq!(row(tool, text).est_tokens_saved, 0, "{tool} {text:?}");
            }
        }
        assert!(row("code_read", "{\"body\":\"fn a() {}\",\"line\":1}").est_tokens_saved > 0);
    }

    #[test]
    fn a_refusal_saves_nothing() {
        let s = refusal("`code_read`: no symbol `x` in `src/a.rs`");
        assert_eq!((s.est_tokens_saved, s.baseline), (0, "tool_error"));
    }

    #[test]
    fn no_baseline_tools_claim_zero_savings() {
        for tool in KNOWN_TOOLS {
            let s = row(tool, ANSWER);
            assert_eq!(s.est_tokens_saved, 0, "{tool} must not claim savings");
            assert_eq!(s.baseline, "no_baseline", "{tool} must label no_baseline");
        }
    }

    /// Every median ratio the committed gate measured for `tool`, over its kinds and corpora.
    fn gate_medians(gate: &serde_json::Value, tool: &str) -> Vec<f64> {
        gate[tool]["kinds"]
            .as_object()
            .into_iter()
            .flat_map(|kinds| kinds.values())
            .flat_map(|corpora| corpora.as_object().into_iter().flat_map(|cells| cells.values()))
            .filter_map(|cell| cell["median_ratio"].as_f64())
            .collect()
    }

    /// Every multiplier is at most the lowest median ratio the committed gate measured for that tool,
    /// and a tool the gate measured above the pass bar everywhere is not left without one.
    #[test]
    fn multipliers_never_exceed_the_committed_gate() {
        let gate: serde_json::Value =
            serde_json::from_str(include_str!("../../benchmarks/eval/results/gate.json")).expect("gate.json");
        for (tool, pct, _) in MEASURED {
            let lowest = gate_medians(&gate, tool).into_iter().fold(f64::INFINITY, f64::min);
            assert!(lowest.is_finite(), "{tool} has no measured median in gate.json");
            let ceiling = ((lowest * 100.0).floor() as u64).max(NO_SAVING_PCT);
            assert!(
                *pct <= ceiling,
                "{tool} claims {pct}% but the gate measured {lowest:.2}x"
            );
        }
        for tool in KNOWN_TOOLS {
            let medians = gate_medians(&gate, tool);
            assert!(
                medians.is_empty() || medians.iter().any(|&ratio| ratio < 1.5),
                "{tool} passed the gate everywhere; give it a multiplier"
            );
        }
    }

    /// Rows written before ADR-0018 carry `domain:mode` keys, and older clients send the
    /// `domain_mode` spelling. Both must keep resolving to the baseline of the tool that replaced
    /// them, so a telemetry file spanning the upgrade still aggregates.
    #[test]
    fn legacy_keys_model_the_baseline_of_the_tool_that_replaced_them() {
        for (legacy, current) in [
            ("code:outline", "code_outline"),
            ("code:symbols", "code_symbols"),
            ("code:expand", "code_read"),
            ("code:references", "code_refs"),
            ("code_references", "code_refs"),
            ("code_callers", "code_refs"),
            ("code:dependents", "code_refs"),
            ("code:find", "find_files"),
            ("code_files", "find_files"),
            ("memory:documents", "docs_search"),
            ("git:recent", "git_log"),
            ("git_blame_symbol", "git_blame"),
            ("git:status", "git_diff"),
            ("admin:status", "index_status"),
        ] {
            let via_legacy = row(legacy, ANSWER);
            let via_current = row(current, ANSWER);
            assert_eq!(via_legacy, via_current, "{legacy} and {current} must share a baseline");
            assert_ne!(via_legacy.baseline, "unclassified", "{legacy} must still resolve");
        }
    }

    /// A name that merely looks like a legacy key must not be coerced into a baseline it was never
    /// modelled for.
    #[test]
    fn names_that_are_not_tools_stay_unclassified() {
        for tool in [
            "shell_exec",
            "code_nonsense",
            "not_a_real_tool",
            "room_broadcast",
            "git:churn",
        ] {
            let s = row(tool, ANSWER);
            assert_eq!(s.baseline, "unclassified", "{tool} must not claim a baseline");
            assert_eq!(s.est_tokens_saved, 0, "{tool} must not claim savings");
        }
    }

    /// Under the heuristic tier (no `tokenizer`), counting the full text is `len / 4`.
    #[cfg(not(feature = "tokenizer"))]
    #[test]
    fn estimate_from_text_is_bytes_over_four_under_heuristic() {
        let text = format!("{{\"body\":\"{}\"}}", "x".repeat(790));
        let s = row("code_outline", &text);
        assert_eq!(s.actual_tokens, 200);
        assert_eq!(s.baseline_tokens, 320);
        assert_eq!(s.est_tokens_saved, 120);
    }
}
