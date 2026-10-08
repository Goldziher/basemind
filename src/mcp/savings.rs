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

use std::borrow::Cow;

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
// monorepo, over the tasks where basemind's answer was correct (recall >= 0.8), rounded down. A mode
// whose measured median is below 1 claims 100: basemind did not beat its baseline there, so it
// claims no saving. The baseline each figure is measured against is the disclosed `baseline` field
// of the row; re-run the eval and update these when a response shape changes.

/// `code:outline` vs reading the whole file. Measured median 1.24x (p90 2.6x) over files with 3-60
/// symbols: a short file costs about as much to outline (symbols plus every import line) as to read.
const OUTLINE_PCT: u64 = 120;

/// `code:symbols` vs `git grep` plus whole-file reads of the (up to three) files holding the hits.
/// Measured median 27.9x (p25 12x, p90 89x); large source files dominate the baseline.
const SYMBOLS_PCT: u64 = 2500;

/// `code:references` vs the `git grep -n -w` line listing. Measured median 1.22x (p90 2.5x).
/// `code:implementations` returns the same response shape and has no eval mode of its own, so it
/// shares this figure.
const REFERENCES_PCT: u64 = 120;

/// `code:callers` vs the `git grep -n -w` line listing. Measured median 0.76x: grouping callers
/// costs more tokens than the bare grep lines, so no saving is claimed.
const CALLERS_PCT: u64 = 100;

/// `code:dependents` vs `git grep -l` of the import lines. Measured median 0.64x (a bare path list
/// is hard to beat), so no saving is claimed.
const DEPENDENTS_PCT: u64 = 100;

/// `code:find` vs `git ls-files | grep`. Measured median 0.30x: the baseline is one path line,
/// basemind's value is the fuzzy/typo-tolerant match, not fewer tokens. No saving is claimed.
const FIND_PCT: u64 = 100;

/// `code:files` vs `find` / `git ls-files` plus a filter. Same response shape as `find` (measured
/// 0.30x there), no eval mode of its own: no saving is claimed.
const LIST_FILES_PCT: u64 = 100;

/// `memory:documents` vs `git grep` of a keyword plus opening the document it points at. Measured
/// median 2.54x (p90 7.5x) on the docs eval, where grep alone returns only bare lines.
const DOCUMENT_READ_PCT: u64 = 250;

/// Web-ingestion baseline multiplier (`web:scrape` / `web:crawl` / `web:map`). The alternative
/// is the agent browsing the page(s) and pasting raw page text into context; the cleaned/extracted
/// response is a fraction of that. Modelled conservatively at 3× the returned payload.
const WEB_INGEST_MULTIPLIER: u64 = 3;

/// Rewrite a tool name spelled `code_outline` to the telemetry key the baseline table is written
/// against (`code:outline`).
///
/// The MCP surface spells a call `domain:mode`; some callers spell it `domain_mode`, since a colon
/// is illegal in the provider tool-name pattern. The two vocabularies are otherwise the same
/// `(domain, mode)` pairs, so one rewrite here keeps every baseline arm single-spelled. A pair that
/// is not a real domain/mode is returned unchanged and falls through to `unclassified`. ~keep
fn canonical_key(tool: &str) -> Cow<'_, str> {
    if tool.contains(':') {
        return Cow::Borrowed(tool);
    }
    let Some((domain, mode)) = tool.split_once('_') else {
        return Cow::Borrowed(tool);
    };
    let known = super::mode::domain_modes()
        .into_iter()
        .any(|(d, modes)| d == domain && modes.contains(&mode));
    if known {
        Cow::Owned(format!("{domain}:{mode}"))
    } else {
        Cow::Borrowed(tool)
    }
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
    let (baseline, baseline_name) = match canonical_key(tool).as_ref() {
        "code:outline" => (scaled(actual, OUTLINE_PCT), "full_file_read"),

        "code:symbols" => (scaled(actual, SYMBOLS_PCT), "grep_plus_read_top_hits"),

        "code:references" | "code:implementations" => (scaled(actual, REFERENCES_PCT), "grep_top_hits"),

        "code:callers" => (scaled(actual, CALLERS_PCT), "grep_top_hits"),

        "code:dependents" => (scaled(actual, DEPENDENTS_PCT), "grep_imports_top_hits"),

        "code:find" => (scaled(actual, FIND_PCT), "git_ls_files_grep"),

        "git:churn" => (actual.saturating_mul(3), "git_log_per_file"),

        "git:symbol_history" => (actual.saturating_mul(4), "per_commit_outline_diff"),

        "code:grep" => (actual, "no_baseline"),

        // `display` and `open` join their read-only siblings here rather than staying unclassified:
        // a rendered view replaces no grep/read baseline, so "saved nothing" is the honest label.
        "graph:calls" | "graph:neighbors" | "graph:path" | "graph:subgraph" | "graph:communities" | "graph:map"
        | "graph:export" | "graph:display" | "graph:open" => (actual, "no_baseline"),

        "memory:documents" => (scaled(actual, DOCUMENT_READ_PCT), "grep_plus_document_read"),

        "code:files" => (scaled(actual, LIST_FILES_PCT), "find_plus_filter"),

        "web:scrape" | "web:crawl" | "web:map" => (actual.saturating_mul(WEB_INGEST_MULTIPLIER), "manual_browse_paste"),

        "memory:get"
        | "memory:put"
        | "memory:list"
        | "memory:search"
        | "memory:delete"
        | "admin:telemetry"
        | "admin:rescan"
        | "admin:cache_stats"
        | "admin:gc"
        | "admin:cache_clear"
        | "admin:status"
        | "admin:repo"
        | "workspace:workspaces"
        | "workspace:worktrees"
        | "workspace:branches"
        | "workspace:claim"
        | "workspace:release"
        | "git:status"
        | "git:recent"
        | "git:touching"
        | "git:by_path"
        | "git:diff"
        | "git:diff_outline"
        | "git:blame"
        | "git:blame_symbol"
        | "git:search" => (actual, "no_baseline"),

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
        let s = estimate_from_text("code:outline", 1_000_000, &"a".repeat(400));
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
        let big = estimate_from_text("code:symbols", 1_000_000, &text);
        let empty = estimate_from_text("code:symbols", 0, &text);
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
    fn find_references_grep_baseline_floors_at_zero_for_empty_corpus() {
        let s = estimate_from_text("code:references", 0, &"a".repeat(200));
        assert_grep_model(&s, "grep_top_hits", 120);
        #[cfg(not(feature = "tokenizer"))]
        {
            assert_eq!(s.actual_tokens, 50);
            assert_eq!(s.baseline_tokens, 60);
            assert_eq!(s.est_tokens_saved, 10);
        }
    }

    #[test]
    fn grep_savings_scale_with_response_not_corpus() {
        let small = estimate_from_text("code:symbols", 1_000_000, &"word ".repeat(80));
        let large = estimate_from_text("code:symbols", 1_000_000, &"word ".repeat(800));
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
            "memory:get",
            "memory:put",
            "admin:status",
            "admin:repo",
            "admin:telemetry",
            "admin:rescan",
            "admin:cache_stats",
            "workspace:worktrees",
            "git:recent",
            "git:touching",
            "git:diff",
            "git:blame",
            "git:status",
            "git:search",
            "code:grep",
            "code_grep",
            "graph:calls",
            "graph:display",
        ] {
            let s = estimate_from_text(tool, 1_000_000, &"a".repeat(500));
            assert_eq!(s.est_tokens_saved, 0, "{tool} must not claim savings");
            assert_eq!(s.baseline, "no_baseline", "{tool} must label no_baseline");
        }
    }

    /// `basemind-agent` registers its LLM-facing tools under `domain_mode` (a colon is illegal in
    /// the provider tool-name pattern) and routes them through this estimator, so the underscore
    /// spelling must reach the same baseline as the `domain:mode` key the MCP surface records.
    /// Without the rewrite the agent TUI's "tokens saved" readout silently reports zero.
    #[test]
    fn agent_tool_names_model_the_same_baseline_as_their_modes() {
        let text = "a".repeat(400);
        for (agent, mode) in [
            ("code_outline", "code:outline"),
            ("code_symbols", "code:symbols"),
            ("code_references", "code:references"),
            ("code_callers", "code:callers"),
            ("code_implementations", "code:implementations"),
            ("code_dependents", "code:dependents"),
            ("code_grep", "code:grep"),
            ("code_files", "code:files"),
            ("graph_calls", "graph:calls"),
            ("git_recent", "git:recent"),
            ("git_blame_symbol", "git:blame_symbol"),
            ("git_diff", "git:diff"),
        ] {
            let via_agent = estimate_from_text(agent, 1_000_000, &text);
            let via_mode = estimate_from_text(mode, 1_000_000, &text);
            assert_eq!(
                via_agent.baseline, via_mode.baseline,
                "{agent} and {mode} must share a baseline"
            );
            assert_eq!(
                via_agent.est_tokens_saved, via_mode.est_tokens_saved,
                "{agent} and {mode} must estimate the same savings"
            );
            assert_ne!(
                via_agent.baseline, "unclassified",
                "{agent} must resolve to a real mode, not fall through"
            );
        }
    }

    /// The rewrite is keyed off the real mode vocabulary, so a name that merely *looks* like
    /// `domain_mode` must not be coerced into a baseline it was never modelled for. `shell_exec` is
    /// the live case: `shell` is a domain but `exec` is not one of its modes.
    #[test]
    fn underscore_names_that_are_not_real_modes_stay_unclassified() {
        for tool in ["shell_exec", "code_nonsense", "not_a_real_tool", "room_broadcast"] {
            let s = estimate_from_text(tool, 1_000_000, &"a".repeat(400));
            assert_eq!(s.baseline, "unclassified", "{tool} must not claim a baseline");
            assert_eq!(s.est_tokens_saved, 0, "{tool} must not claim savings");
        }
    }

    #[test]
    fn search_documents_models_grep_plus_document_read_at_2_5x() {
        let s = estimate_from_text("memory:documents", 1_000_000, &"a".repeat(400));
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

    /// Modes whose eval-measured median ratio is below 1 (basemind's response costs more tokens than
    /// the plain `rg` / `git ls-files` baseline) keep their disclosed baseline but claim no saving.
    #[test]
    fn modes_that_did_not_beat_their_baseline_claim_no_saving() {
        for (tool, baseline) in [
            ("code:callers", "grep_top_hits"),
            ("code:dependents", "grep_imports_top_hits"),
            ("code:find", "git_ls_files_grep"),
            ("code:files", "find_plus_filter"),
        ] {
            let s = estimate_from_text(tool, 1_000_000, &"a".repeat(400));
            assert_eq!(s.baseline, baseline, "{tool} baseline label");
            assert_eq!(s.baseline_tokens, s.actual_tokens, "{tool} must model a 1x baseline");
            assert_eq!(s.est_tokens_saved, 0, "{tool} must not claim savings");
        }
    }

    #[test]
    fn web_ingest_models_manual_browse_paste_at_3x() {
        for tool in ["web:scrape", "web:crawl", "web:map"] {
            let s = estimate_from_text(tool, 1_000_000, &"a".repeat(400));
            assert_eq!(s.baseline, "manual_browse_paste", "{tool} baseline name");
            assert_eq!(
                s.baseline_tokens,
                s.actual_tokens.saturating_mul(3),
                "{tool} multiplier"
            );
            assert_eq!(
                s.est_tokens_saved,
                s.baseline_tokens.saturating_sub(s.actual_tokens),
                "{tool} savings"
            );
            #[cfg(not(feature = "tokenizer"))]
            {
                assert_eq!(s.actual_tokens, 100, "{tool} actual");
                assert_eq!(s.baseline_tokens, 300, "{tool} baseline");
                assert_eq!(s.est_tokens_saved, 200, "{tool} saved");
            }
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
        let s = estimate_from_text("code:outline", 0, &"x".repeat(800));
        assert_eq!(s.actual_tokens, 200);
        assert_eq!(s.baseline_tokens, 240);
        assert_eq!(s.est_tokens_saved, 40);
    }
}
