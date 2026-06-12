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

/// `bytes / 4` token estimate, saturating.
fn bytes_to_tokens(bytes: u64) -> u64 {
    bytes / 4
}

/// Estimate baseline + actual tokens for one tool call.
///
/// `corpus_bytes` is the total byte count of every indexed file (held on `ServerState` and
/// recomputed after each rescan). Used to model the cost of a hypothetical grep across the
/// repo. Pass 0 when unknown — the savings number degrades gracefully to 0.
pub fn estimate(tool: &str, corpus_bytes: u64, resp_bytes: u64) -> SavingsRow {
    let actual = bytes_to_tokens(resp_bytes);

    let (baseline, baseline_name) = match tool {
        // Outline replaces a full Read of the file. We don't know which file
        // without re-deserialising params, so use the response bytes as a
        // floor — the underlying file is typically 3–10× the outline summary.
        "outline" => (actual.saturating_mul(5), "full_file_read"),

        // Symbol-name search would otherwise be `grep -r <needle>` followed by
        // reading the top hits. Assume 5% of corpus_bytes for the grep
        // (rough — grep reads everything but emits little) + the actual hit
        // payload as the "Read top results" cost.
        "search_symbols" => (
            bytes_to_tokens(corpus_bytes / 20).saturating_add(actual),
            "grep_plus_read_top_hits",
        ),

        // Reference / caller lookups: same grep model as search_symbols but
        // without the Read step — `find_references` already returns the call
        // sites inline.
        "find_references" | "find_callers" => {
            (bytes_to_tokens(corpus_bytes / 20), "grep_across_corpus")
        }

        // Implementation lookups: alternative is `rg 'impl.*Trait'` / `grep class.*extends`
        // plus manual filtering across languages. Same grep ratio as find_references — the
        // corpus scan cost dominates and the response is already the filtered result.
        "find_implementations" => {
            (bytes_to_tokens(corpus_bytes / 20), "grep_across_corpus")
        }

        // Dependents = grep imports across the corpus. Imports are sparse so
        // the grep ratio is lower than for name search.
        "dependents" => (
            bytes_to_tokens(corpus_bytes / 30),
            "grep_imports_across_corpus",
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
    fn search_symbols_uses_corpus_size() {
        let s = estimate("search_symbols", 1_000_000, 400);
        // 1_000_000 / 20 / 4 = 12_500 grep + 100 actual baseline; 100 actual
        assert_eq!(s.baseline_tokens, 12_600);
        assert_eq!(s.actual_tokens, 100);
        assert_eq!(s.est_tokens_saved, 12_500);
        assert_eq!(s.baseline, "grep_plus_read_top_hits");
    }

    #[test]
    fn find_references_grep_baseline_floors_at_zero_for_empty_corpus() {
        let s = estimate("find_references", 0, 200);
        assert_eq!(s.baseline_tokens, 0);
        assert_eq!(s.est_tokens_saved, 0); // saturating_sub
        assert_eq!(s.baseline, "grep_across_corpus");
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
