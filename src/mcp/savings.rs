//! Heuristic estimator for "how many tokens did this basemind tool call save the agent vs the
//! grep + Read baseline?". Honest about being a heuristic — every row carries the baseline name
//! so the dashboard can disclose the assumption.
//!
//! Token counting has two tiers. When the **full response text** is in hand, the figures route
//! through [`super::tokens::count_tokens`] — a real o200k (gpt-4o) tokenizer under the `documents`
//! feature, a `bytes / 4` heuristic otherwise. When only a **byte length** is available (the live
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

/// Grep-style name search (`search_symbols`, `find_references`, `find_callers`,
/// `find_implementations`): the agent pays for the grep output (≈ the matching hits we
/// already return) plus opening a few top hits to confirm them. Modelled as the response
/// payload times this multiplier — corpus-independent, since a real `rg` emits matching
/// lines, not whole files, and the agent reads only the top results.
const GREP_READ_MULTIPLIER: u64 = 3;

/// `dependents` baseline multiplier. Imports are sparse and a reverse-import lookup leaves
/// less follow-up file reading than a name search, so this is lower than `GREP_READ_MULTIPLIER`.
const DEPENDENTS_READ_MULTIPLIER: u64 = 2;

/// `search_documents` baseline multiplier. The agent's alternative is reading whole documents
/// to find the relevant passages; the response returns just the matching chunks. Modelled like
/// `outline` (~5×) — the source documents are typically several times the extracted snippet.
const DOCUMENT_READ_MULTIPLIER: u64 = 5;

/// `list_files` baseline multiplier. The alternative is shelling out to `find` / `ls -R` and
/// then reading the (unfiltered, noisier) listing the agent must scan by hand. A modest 2× —
/// basemind returns the already-filtered set, saving the agent the extra listing it reads.
const LIST_FILES_READ_MULTIPLIER: u64 = 2;

/// Web-ingestion baseline multiplier (`web_scrape` / `web_crawl` / `web_map`). The alternative
/// is the agent browsing the page(s) and pasting raw page text into context; the cleaned/extracted
/// response is a fraction of that. Modelled conservatively at 3× the returned payload.
const WEB_INGEST_MULTIPLIER: u64 = 3;

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
    let (baseline, baseline_name) = match tool {
        "outline" => (actual.saturating_mul(5), "full_file_read"),

        "search_symbols" => (actual.saturating_mul(GREP_READ_MULTIPLIER), "grep_plus_read_top_hits"),

        "find_references" | "find_callers" => (actual.saturating_mul(GREP_READ_MULTIPLIER), "grep_top_hits"),

        "find_implementations" => (actual.saturating_mul(GREP_READ_MULTIPLIER), "grep_top_hits"),

        "dependents" => (
            actual.saturating_mul(DEPENDENTS_READ_MULTIPLIER),
            "grep_imports_top_hits",
        ),

        "hot_files" => (actual.saturating_mul(3), "git_log_per_file"),

        "symbol_history" => (actual.saturating_mul(4), "per_commit_outline_diff"),

        "workspace_grep" => (actual, "no_baseline"),

        "call_graph" => (actual, "no_baseline"),

        "architecture_map" => (actual, "no_baseline"),

        "search_documents" => (actual.saturating_mul(DOCUMENT_READ_MULTIPLIER), "full_document_read"),

        "list_files" => (actual.saturating_mul(LIST_FILES_READ_MULTIPLIER), "find_plus_filter"),

        "web_scrape" | "web_crawl" | "web_map" => (actual.saturating_mul(WEB_INGEST_MULTIPLIER), "manual_browse_paste"),

        "memory_get"
        | "memory_put"
        | "memory_list"
        | "memory_search"
        | "memory_delete"
        | "telemetry_summary"
        | "rescan"
        | "cache_stats"
        | "cache_gc"
        | "cache_clear"
        | "status"
        | "repo_info"
        | "working_tree_status"
        | "recent_changes"
        | "commits_touching"
        | "find_commits_by_path"
        | "diff_file"
        | "diff_outline"
        | "blame_file"
        | "blame_symbol" => (actual, "no_baseline"),

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
    fn assert_grep_model(s: &SavingsRow, expected_baseline: &str) {
        assert_eq!(s.baseline, expected_baseline);
        assert_eq!(s.baseline_tokens, s.actual_tokens.saturating_mul(GREP_READ_MULTIPLIER));
        assert_eq!(s.est_tokens_saved, s.baseline_tokens.saturating_sub(s.actual_tokens));
    }

    #[test]
    fn outline_baseline_is_5x_response() {
        let s = estimate_from_text("outline", 1_000_000, &"a".repeat(400));
        assert_eq!(s.baseline_tokens, s.actual_tokens.saturating_mul(5));
        assert_eq!(s.baseline, "full_file_read");
        #[cfg(not(feature = "documents"))]
        {
            assert_eq!(s.actual_tokens, 100);
            assert_eq!(s.baseline_tokens, 500);
            assert_eq!(s.est_tokens_saved, 400);
        }
    }

    #[test]
    fn search_symbols_savings_independent_of_corpus() {
        let text = "a".repeat(400);
        let big = estimate_from_text("search_symbols", 1_000_000, &text);
        let empty = estimate_from_text("search_symbols", 0, &text);
        assert_eq!(big.est_tokens_saved, empty.est_tokens_saved);
        assert_grep_model(&big, "grep_plus_read_top_hits");
        #[cfg(not(feature = "documents"))]
        {
            assert_eq!(big.actual_tokens, 100);
            assert_eq!(big.baseline_tokens, 300);
            assert_eq!(big.est_tokens_saved, 200);
        }
    }

    #[test]
    fn find_references_grep_baseline_floors_at_zero_for_empty_corpus() {
        let s = estimate_from_text("find_references", 0, &"a".repeat(200));
        assert_grep_model(&s, "grep_top_hits");
        #[cfg(not(feature = "documents"))]
        {
            assert_eq!(s.actual_tokens, 50);
            assert_eq!(s.baseline_tokens, 150);
            assert_eq!(s.est_tokens_saved, 100);
        }
    }

    #[test]
    fn grep_savings_scale_with_response_not_corpus() {
        let small = estimate_from_text("search_symbols", 1_000_000, &"word ".repeat(80));
        let large = estimate_from_text("search_symbols", 1_000_000, &"word ".repeat(800));
        assert!(
            large.est_tokens_saved > small.est_tokens_saved,
            "bigger response must yield bigger savings: {} !> {}",
            large.est_tokens_saved,
            small.est_tokens_saved
        );
        #[cfg(not(feature = "documents"))]
        assert_eq!(large.est_tokens_saved, 2_000);
    }

    #[test]
    fn no_baseline_tools_claim_zero_savings() {
        for tool in [
            "memory_get",
            "memory_put",
            "status",
            "repo_info",
            "telemetry_summary",
            "rescan",
            "cache_stats",
            "recent_changes",
            "commits_touching",
            "diff_file",
            "blame_file",
            "working_tree_status",
            "workspace_grep",
            "call_graph",
        ] {
            let s = estimate_from_text(tool, 1_000_000, &"a".repeat(500));
            assert_eq!(s.est_tokens_saved, 0, "{tool} must not claim savings");
            assert_eq!(s.baseline, "no_baseline", "{tool} must label no_baseline");
        }
    }

    #[test]
    fn search_documents_models_full_document_read_at_5x() {
        let s = estimate_from_text("search_documents", 1_000_000, &"a".repeat(400));
        assert_eq!(s.baseline, "full_document_read");
        assert_eq!(s.baseline_tokens, s.actual_tokens.saturating_mul(5));
        assert_eq!(s.est_tokens_saved, s.baseline_tokens.saturating_sub(s.actual_tokens));
        #[cfg(not(feature = "documents"))]
        {
            assert_eq!(s.actual_tokens, 100);
            assert_eq!(s.baseline_tokens, 500);
            assert_eq!(s.est_tokens_saved, 400);
        }
    }

    #[test]
    fn list_files_models_find_plus_filter_at_2x() {
        let s = estimate_from_text("list_files", 1_000_000, &"a".repeat(400));
        assert_eq!(s.baseline, "find_plus_filter");
        assert_eq!(s.baseline_tokens, s.actual_tokens.saturating_mul(2));
        assert_eq!(s.est_tokens_saved, s.baseline_tokens.saturating_sub(s.actual_tokens));
        #[cfg(not(feature = "documents"))]
        {
            assert_eq!(s.actual_tokens, 100);
            assert_eq!(s.baseline_tokens, 200);
            assert_eq!(s.est_tokens_saved, 100);
        }
    }

    #[test]
    fn web_ingest_models_manual_browse_paste_at_3x() {
        for tool in ["web_scrape", "web_crawl", "web_map"] {
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
            #[cfg(not(feature = "documents"))]
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
    #[cfg(not(feature = "documents"))]
    #[test]
    fn estimate_from_text_is_bytes_over_four_under_heuristic() {
        let s = estimate_from_text("outline", 0, &"x".repeat(800));
        assert_eq!(s.actual_tokens, 200);
        assert_eq!(s.baseline_tokens, 1_000);
        assert_eq!(s.est_tokens_saved, 800);
    }
}
