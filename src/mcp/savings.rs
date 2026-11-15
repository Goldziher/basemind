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

/// `bytes / 4` token estimate, saturating. The byte-only fallback used wherever the full text
/// is NOT in hand — only a byte length. `pub(super)` so the budget helper ([`super::budget`])
/// shares the exact same bytes→token factor for its per-item ranking heuristic.
pub(super) fn bytes_to_tokens(bytes: u64) -> u64 {
    bytes / 4
}

/// Real token count of `text`, routed through [`super::tokens::count_tokens`]: a true o200k
/// (gpt-4o) tokenizer under the `documents` feature, `bytes / 4` otherwise. Use this — not
/// [`bytes_to_tokens`] — wherever the full response text is available, so telemetry reports
/// honest token figures when a tokenizer is compiled in.
fn tokens_for_text(text: &str) -> u64 {
    super::tokens::count_tokens(text)
}

/// Scale a response's token count by a baseline multiplier expressed in percent (`120` = 1.2x).
/// Multipliers are fractional because several lookups save little: the baseline is a plain `rg`
/// output, which is already compact.
fn scaled(actual: u64, pct: u64) -> u64 {
    actual.saturating_mul(pct) / 100
}

// Baseline multipliers, in percent. Each is the MEDIAN token ratio (baseline tokens / basemind
// response tokens) measured by `basemind admin eval` (benchmarks/eval) on a ~63k-file Python/TS
// monorepo, over the tasks where basemind's answer was correct (recall >= 0.8), rounded down. A tool
// whose measured median is below 1 claims 100: basemind did not beat its baseline there, so it
// claims no saving. The baseline each figure is measured against is the disclosed `baseline` field
// of the row; re-run the eval and update these when a response shape changes.

/// `code_outline` vs reading the whole file. Measured median 1.24x (p90 2.6x) over files with 3-60
/// symbols: a short file costs about as much to outline (symbols plus every import line) as to read.
const OUTLINE_PCT: u64 = 120;

/// `code_symbols` vs `git grep` plus whole-file reads of the (up to three) files holding the hits.
/// Measured median 27.9x (p25 12x, p90 89x); large source files dominate the baseline.
const SYMBOLS_PCT: u64 = 2500;

/// `code_refs` vs the `git grep -n -w` line listing. Its shapes measured 1.22x (call sites of a
/// name), 0.76x (callers of one definition) and 0.64x (importers); one telemetry key covers all
/// of them, so the tool claims no saving rather than the best shape's figure.
const REFS_PCT: u64 = 100;

/// `find_files` vs `git ls-files | grep`. Measured median 0.30x: the baseline is one path line,
/// basemind's value is the fuzzy/typo-tolerant match, not fewer tokens. No saving is claimed.
const FIND_PCT: u64 = 100;

/// `docs_search` vs `git grep` of a keyword plus opening the document it points at. Measured
/// median 2.54x (p90 7.5x) on the docs eval, where grep alone returns only bare lines.
const DOCUMENT_READ_PCT: u64 = 250;

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
/// ([`bytes_to_tokens`]) remains for paths that hold only a byte length, e.g. the budget loop.
///
/// `corpus_bytes` is the total byte count of every indexed file (held on `ServerState` and
/// recomputed after each rescan). Retained for signature stability and potential future
/// per-tool models; the grep-style baselines are now corpus-independent (derived from the
/// response payload), so this argument currently goes unused.
pub fn estimate_from_text(tool: &str, _corpus_bytes: u64, resp_text: &str) -> SavingsRow {
    let actual = tokens_for_text(resp_text);
    let (baseline, baseline_name) = match canonical_key(tool) {
        "code_outline" => (scaled(actual, OUTLINE_PCT), "full_file_read"),

        "code_symbols" => (scaled(actual, SYMBOLS_PCT), "grep_plus_read_top_hits"),

        "code_refs" => (scaled(actual, REFS_PCT), "grep_top_hits"),

        "find_files" => (scaled(actual, FIND_PCT), "git_ls_files_grep"),

        "docs_search" => (scaled(actual, DOCUMENT_READ_PCT), "grep_plus_document_read"),

        "code_grep"
        | "code_read"
        | "code_search"
        | "git_log"
        | "git_blame"
        | "git_diff"
        | "memory_save"
        | "memory_search"
        | "index_status"
        | "index_rescan"
        | "workspace:workspaces"
        | "workspace:worktrees"
        | "workspace:branches"
        | "workspace:claim"
        | "workspace:release" => (actual, "no_baseline"),

        _ => (actual, "unclassified"),
    };

    SavingsRow {
        baseline_tokens: baseline,
        actual_tokens: actual,
        est_tokens_saved: baseline.saturating_sub(actual),
        baseline: baseline_name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Baseline-model assertions that hold for both tiers: the per-tool multiplier and the
    /// saturating-subtraction savings, expressed relative to whatever `actual` was counted.
    /// Used by the structural tests so they pass under `documents` (real o200k) too.
    fn assert_grep_model(s: &SavingsRow, expected_baseline: &str, pct: u64) {
        assert_eq!(s.baseline, expected_baseline);
        assert_eq!(s.baseline_tokens, scaled(s.actual_tokens, pct));
        assert_eq!(s.est_tokens_saved, s.baseline_tokens.saturating_sub(s.actual_tokens));
    }

    #[test]
    fn outline_baseline_is_1_2x_response() {
        let s = estimate_from_text("code_outline", 1_000_000, &"a".repeat(400));
        assert_eq!(s.baseline_tokens, scaled(s.actual_tokens, 120));
        assert_eq!(s.baseline, "full_file_read");
        #[cfg(not(feature = "tokenizer"))]
        {
            assert_eq!(s.actual_tokens, 100);
            assert_eq!(s.baseline_tokens, 120);
            assert_eq!(s.est_tokens_saved, 20);
        }
    }

    #[test]
    fn search_symbols_savings_independent_of_corpus() {
        let text = "a".repeat(400);
        let big = estimate_from_text("code_symbols", 1_000_000, &text);
        let empty = estimate_from_text("code_symbols", 0, &text);
        assert_eq!(big.est_tokens_saved, empty.est_tokens_saved);
        assert_grep_model(&big, "grep_plus_read_top_hits", 2500);
        #[cfg(not(feature = "tokenizer"))]
        {
            assert_eq!(big.actual_tokens, 100);
            assert_eq!(big.baseline_tokens, 2_500);
            assert_eq!(big.est_tokens_saved, 2_400);
        }
    }

    #[test]
    fn grep_savings_scale_with_response_not_corpus() {
        let small = estimate_from_text("code_symbols", 1_000_000, &"word ".repeat(80));
        let large = estimate_from_text("code_symbols", 1_000_000, &"word ".repeat(800));
        assert!(
            large.est_tokens_saved > small.est_tokens_saved,
            "bigger response must yield bigger savings: {} !> {}",
            large.est_tokens_saved,
            small.est_tokens_saved
        );
        #[cfg(not(feature = "tokenizer"))]
        assert_eq!(large.est_tokens_saved, 24_000);
    }

    #[test]
    fn no_baseline_tools_claim_zero_savings() {
        for tool in [
            "memory_search",
            "memory_save",
            "index_status",
            "index_rescan",
            "workspace:worktrees",
            "git_log",
            "git_diff",
            "git_blame",
            "code_grep",
            "code_read",
            "code_search",
        ] {
            let s = estimate_from_text(tool, 1_000_000, &"a".repeat(500));
            assert_eq!(s.est_tokens_saved, 0, "{tool} must not claim savings");
            assert_eq!(s.baseline, "no_baseline", "{tool} must label no_baseline");
        }
    }

    /// Rows written before ADR-0018 carry `domain:mode` keys, and older clients send the
    /// `domain_mode` spelling. Both must keep resolving to the baseline of the tool that replaced
    /// them, so a telemetry file spanning the upgrade still aggregates.
    #[test]
    fn legacy_keys_model_the_baseline_of_the_tool_that_replaced_them() {
        let text = "a".repeat(400);
        for (legacy, current) in [
            ("code:outline", "code_outline"),
            ("code:symbols", "code_symbols"),
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
            let via_legacy = estimate_from_text(legacy, 1_000_000, &text);
            let via_current = estimate_from_text(current, 1_000_000, &text);
            assert_eq!(
                via_legacy.baseline, via_current.baseline,
                "{legacy} and {current} must share a baseline"
            );
            assert_eq!(
                via_legacy.est_tokens_saved, via_current.est_tokens_saved,
                "{legacy} and {current} must estimate the same savings"
            );
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
            let s = estimate_from_text(tool, 1_000_000, &"a".repeat(400));
            assert_eq!(s.baseline, "unclassified", "{tool} must not claim a baseline");
            assert_eq!(s.est_tokens_saved, 0, "{tool} must not claim savings");
        }
    }

    #[test]
    fn search_documents_models_grep_plus_document_read_at_2_5x() {
        let s = estimate_from_text("docs_search", 1_000_000, &"a".repeat(400));
        assert_eq!(s.baseline, "grep_plus_document_read");
        assert_eq!(s.baseline_tokens, scaled(s.actual_tokens, 250));
        assert_eq!(s.est_tokens_saved, s.baseline_tokens.saturating_sub(s.actual_tokens));
        #[cfg(not(feature = "tokenizer"))]
        {
            assert_eq!(s.actual_tokens, 100);
            assert_eq!(s.baseline_tokens, 250);
            assert_eq!(s.est_tokens_saved, 150);
        }
    }

    /// Tools whose eval-measured median ratio is below 1 (basemind's response costs more tokens than
    /// the plain `rg` / `git ls-files` baseline) keep their disclosed baseline but claim no saving.
    #[test]
    fn tools_that_did_not_beat_their_baseline_claim_no_saving() {
        for (tool, baseline) in [("code_refs", "grep_top_hits"), ("find_files", "git_ls_files_grep")] {
            let s = estimate_from_text(tool, 1_000_000, &"a".repeat(400));
            assert_eq!(s.baseline, baseline, "{tool} baseline label");
            assert_eq!(s.baseline_tokens, s.actual_tokens, "{tool} must model a 1x baseline");
            assert_eq!(s.est_tokens_saved, 0, "{tool} must not claim savings");
        }
    }

    #[test]
    fn unknown_tool_is_unclassified() {
        let s = estimate_from_text("not_a_real_tool", 1_000_000, &"a".repeat(100));
        assert_eq!(s.baseline, "unclassified");
        assert_eq!(s.est_tokens_saved, 0);
    }

    /// Under the heuristic tier (no `documents`), counting the full text is byte-for-byte
    /// `len / 4` — the telemetry numbers are identical to the old `bytes / 4` estimate.
    #[cfg(not(feature = "tokenizer"))]
    #[test]
    fn estimate_from_text_is_bytes_over_four_under_heuristic() {
        let s = estimate_from_text("code_outline", 0, &"x".repeat(800));
        assert_eq!(s.actual_tokens, 200);
        assert_eq!(s.baseline_tokens, 240);
        assert_eq!(s.est_tokens_saved, 40);
    }
}
