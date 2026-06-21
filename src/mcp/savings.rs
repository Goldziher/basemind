//! Heuristic estimator for "how many tokens did this basemind tool call save the agent vs the
//! grep + Read baseline?". Honest about being a heuristic — every row carries the baseline name
//! so the dashboard can disclose the assumption.
//!
//! Bytes → tokens uses the standard `bytes / 4` rule of thumb for English source code with the
//! Claude tokenizer. Same factor as basemind's existing scan-cost reporting.

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

/// `bytes / 4` token estimate, saturating. `pub(super)` so the budget helper
/// ([`super::budget`]) shares the exact same bytes→token factor the telemetry estimator uses.
pub(super) fn bytes_to_tokens(bytes: u64) -> u64 {
    bytes / 4
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

/// Estimate baseline + actual tokens for one tool call.
///
/// `corpus_bytes` is the total byte count of every indexed file (held on `ServerState` and
/// recomputed after each rescan). Retained for signature stability and potential future
/// per-tool models; the grep-style baselines are now corpus-independent (derived from the
/// response payload), so this argument currently goes unused.
pub fn estimate(tool: &str, _corpus_bytes: u64, resp_bytes: u64) -> SavingsRow {
    let actual = bytes_to_tokens(resp_bytes);

    let (baseline, baseline_name) = match tool {
        // Outline replaces a full Read of the file. We don't know which file
        // without re-deserialising params, so use the response bytes as a
        // floor — the underlying file is typically 3–10× the outline summary.
        "outline" => (actual.saturating_mul(5), "full_file_read"),

        // Symbol-name search would otherwise be `grep -r <needle>` followed by
        // reading the top hits. The grep emits the matching lines (≈ our response
        // payload) and the agent reads a few top files to confirm — modelled as
        // the response times GREP_READ_MULTIPLIER, independent of corpus size.
        "search_symbols" => (
            actual.saturating_mul(GREP_READ_MULTIPLIER),
            "grep_plus_read_top_hits",
        ),

        // Reference / caller lookups: same grep-output-plus-confirm model as
        // search_symbols. `find_references` already returns the call sites inline,
        // so the payload is the grep output and the multiplier covers reading a
        // few sites to confirm. Corpus-independent.
        "find_references" | "find_callers" => {
            (actual.saturating_mul(GREP_READ_MULTIPLIER), "grep_top_hits")
        }

        // Implementation lookups: alternative is `rg 'impl.*Trait'` / `grep class.*extends`
        // plus manual filtering across languages. Same grep-output-plus-confirm model —
        // the response is the filtered result, the multiplier covers confirming a few hits.
        "find_implementations" => {
            (actual.saturating_mul(GREP_READ_MULTIPLIER), "grep_top_hits")
        }

        // Dependents = grep imports across the corpus. Imports are sparse and the
        // result needs less follow-up reading than a name search, so it uses the
        // smaller DEPENDENTS_READ_MULTIPLIER. Corpus-independent.
        "dependents" => (
            actual.saturating_mul(DEPENDENTS_READ_MULTIPLIER),
            "grep_imports_top_hits",
        ),

        // Hot files: the agent would otherwise iterate `git log` per file,
        // which is many round-trips. Model conservatively: 100 commits × 200
        // bytes each per file in the result.
        "hot_files" => (actual.saturating_mul(3), "git_log_per_file"),

        // symbol_history: avoiding many `git blame` + tree-sitter diffs. Same
        // order of magnitude as outline savings.
        "symbol_history" => (actual.saturating_mul(4), "per_commit_outline_diff"),

        // workspace_grep: alternative is shelling out to `rg`/`grep`. The agent
        // would spend tokens reading grep output from stdout; the MCP response is
        // comparable in size, so no honest savings number. Record but don't claim.
        "workspace_grep" => (actual, "no_baseline"),

        // call_graph: alternative is many manual `find_references` / `find_callers`
        // calls plus building the DAG in the agent's head. There's no clean grep
        // baseline — substring-grepping for a callee leaves the "who calls *that*"
        // step to the agent. Record the call, claim zero savings.
        "call_graph" => (actual, "no_baseline"),

        // Tools where basemind is the only practical path — no honest grep+read
        // baseline. Record the call but don't claim savings.
        "memory_get"
        | "memory_put"
        | "memory_list"
        | "memory_search"
        | "memory_delete"
        | "search_documents"
        | "telemetry_summary"
        | "rescan"
        | "cache_stats"
        | "cache_gc"
        | "cache_clear"
        | "status"
        | "repo_info"
        | "list_files"
        | "working_tree_status"
        | "recent_changes"
        | "commits_touching"
        | "find_commits_by_path"
        | "diff_file"
        | "diff_outline"
        | "blame_file"
        | "blame_symbol"
        // Web ingestion: the alternative ("agent browses + copies the text in")
        // isn't a tokenizable baseline. Surface the calls in telemetry but
        // claim no savings.
        | "web_scrape"
        | "web_crawl"
        | "web_map" => (actual, "no_baseline"),

        // Unknown tool name (e.g. an upstream addition we haven't classified
        // yet) — be conservative.
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

    #[test]
    fn outline_baseline_is_5x_response() {
        let s = estimate("outline", 1_000_000, 400);
        assert_eq!(s.actual_tokens, 100);
        assert_eq!(s.baseline_tokens, 500);
        assert_eq!(s.est_tokens_saved, 400);
        assert_eq!(s.baseline, "full_file_read");
    }

    #[test]
    fn search_symbols_savings_independent_of_corpus() {
        // Same response payload, wildly different corpus sizes → identical savings.
        let big = estimate("search_symbols", 1_000_000, 400);
        let empty = estimate("search_symbols", 0, 400);
        assert_eq!(big.est_tokens_saved, empty.est_tokens_saved);
        // 400 bytes → 100 actual tokens; baseline = 100 * 3 = 300; saved = 200.
        assert_eq!(big.actual_tokens, 100);
        assert_eq!(big.baseline_tokens, 300);
        assert_eq!(big.est_tokens_saved, 200);
        assert_eq!(big.baseline, "grep_plus_read_top_hits");
    }

    #[test]
    fn find_references_grep_baseline_floors_at_zero_for_empty_corpus() {
        // Corpus is now irrelevant; savings derive from the response payload.
        let s = estimate("find_references", 0, 200);
        // 200 bytes → 50 actual; baseline = 50 * 3 = 150; saved = 100.
        assert_eq!(s.actual_tokens, 50);
        assert_eq!(s.baseline_tokens, 150);
        assert_eq!(s.est_tokens_saved, 100);
        assert_eq!(s.baseline, "grep_top_hits");
    }

    #[test]
    fn grep_savings_scale_with_response_not_corpus() {
        // Larger hit payload → larger savings, holding corpus fixed.
        let small = estimate("search_symbols", 1_000_000, 400);
        let large = estimate("search_symbols", 1_000_000, 4_000);
        assert!(
            large.est_tokens_saved > small.est_tokens_saved,
            "bigger response must yield bigger savings: {} !> {}",
            large.est_tokens_saved,
            small.est_tokens_saved
        );
        // 4_000 bytes → 1_000 actual; baseline = 3_000; saved = 2_000.
        assert_eq!(large.est_tokens_saved, 2_000);
    }

    #[test]
    fn no_baseline_tools_claim_zero_savings() {
        for tool in [
            "memory_get",
            "memory_put",
            "search_documents",
            "status",
            "web_scrape",
            "web_crawl",
            "web_map",
            "workspace_grep",
        ] {
            let s = estimate(tool, 1_000_000, 500);
            assert_eq!(s.est_tokens_saved, 0, "{tool} must not claim savings");
            assert_eq!(s.baseline, "no_baseline", "{tool} must label no_baseline");
        }
    }

    #[test]
    fn unknown_tool_is_unclassified() {
        let s = estimate("not_a_real_tool", 1_000_000, 100);
        assert_eq!(s.baseline, "unclassified");
        assert_eq!(s.est_tokens_saved, 0);
    }
}
