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

/// Grep-style name search (`code:symbols`, `code:references`, `code:callers`,
/// `code:implementations`): the agent pays for the grep output (≈ the matching hits we
/// already return) plus opening a few top hits to confirm them. Modelled as the response
/// payload times this multiplier — corpus-independent, since a real `rg` emits matching
/// lines, not whole files, and the agent reads only the top results.
const GREP_READ_MULTIPLIER: u64 = 3;

/// `dependents` baseline multiplier. Imports are sparse and a reverse-import lookup leaves
/// less follow-up file reading than a name search, so this is lower than `GREP_READ_MULTIPLIER`.
const DEPENDENTS_READ_MULTIPLIER: u64 = 2;

/// `memory:documents` baseline multiplier. The agent's alternative is reading whole documents
/// to find the relevant passages; the response returns just the matching chunks. Modelled like
/// `outline` (~5×) — the source documents are typically several times the extracted snippet.
const DOCUMENT_READ_MULTIPLIER: u64 = 5;

/// `list_files` baseline multiplier. The alternative is shelling out to `find` / `ls -R` and
/// then reading the (unfiltered, noisier) listing the agent must scan by hand. A modest 2× —
/// basemind returns the already-filtered set, saving the agent the extra listing it reads.
const LIST_FILES_READ_MULTIPLIER: u64 = 2;

/// Web-ingestion baseline multiplier (`web:scrape` / `web:crawl` / `web:map`). The alternative
/// is the agent browsing the page(s) and pasting raw page text into context; the cleaned/extracted
/// response is a fraction of that. Modelled conservatively at 3× the returned payload.
const WEB_INGEST_MULTIPLIER: u64 = 3;

/// Rewrite an agent-registered tool name (`code_outline`) to the telemetry key the baseline table
/// is written against (`code:outline`).
///
/// `basemind-agent` registers its LLM-facing tools under bare snake_case names because the provider
/// tool-name pattern (`^[a-zA-Z0-9_-]{1,128}$`) rejects the colon the MCP surface uses, and it
/// routes them through this estimator via `agent_api::estimate_tokens_saved`. The two vocabularies
/// are otherwise the same `(domain, mode)` pairs, so one rewrite here keeps every baseline arm
/// single-spelled. Carrying both spellings per arm is what previously made a rename able to zero
/// the agent TUI's "tokens saved" readout in production without failing a test. A pair that is not
/// a real domain/mode is returned unchanged and falls through to `unclassified`. ~keep
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
        "code:outline" => (actual.saturating_mul(5), "full_file_read"),

        "code:symbols" => (actual.saturating_mul(GREP_READ_MULTIPLIER), "grep_plus_read_top_hits"),

        "code:references" | "code:callers" => (actual.saturating_mul(GREP_READ_MULTIPLIER), "grep_top_hits"),

        "code:implementations" => (actual.saturating_mul(GREP_READ_MULTIPLIER), "grep_top_hits"),

        "code:dependents" => (
            actual.saturating_mul(DEPENDENTS_READ_MULTIPLIER),
            "grep_imports_top_hits",
        ),

        "git:churn" => (actual.saturating_mul(3), "git_log_per_file"),

        "git:symbol_history" => (actual.saturating_mul(4), "per_commit_outline_diff"),

        "code:grep" => (actual, "no_baseline"),

        // `display` and `open` join their read-only siblings here rather than staying unclassified:
        // a rendered view replaces no grep/read baseline, so "saved nothing" is the honest label.
        "graph:calls" | "graph:neighbors" | "graph:path" | "graph:subgraph" | "graph:communities" | "graph:map"
        | "graph:export" | "graph:display" | "graph:open" => (actual, "no_baseline"),

        "memory:documents" => (actual.saturating_mul(DOCUMENT_READ_MULTIPLIER), "full_document_read"),

        "code:files" => (actual.saturating_mul(LIST_FILES_READ_MULTIPLIER), "find_plus_filter"),

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
    fn assert_grep_model(s: &SavingsRow, expected_baseline: &str) {
        assert_eq!(s.baseline, expected_baseline);
        assert_eq!(s.baseline_tokens, s.actual_tokens.saturating_mul(GREP_READ_MULTIPLIER));
        assert_eq!(s.est_tokens_saved, s.baseline_tokens.saturating_sub(s.actual_tokens));
    }

    #[test]
    fn outline_baseline_is_5x_response() {
        let s = estimate_from_text("code:outline", 1_000_000, &"a".repeat(400));
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
        let big = estimate_from_text("code:symbols", 1_000_000, &text);
        let empty = estimate_from_text("code:symbols", 0, &text);
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
        let s = estimate_from_text("code:references", 0, &"a".repeat(200));
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
        let small = estimate_from_text("code:symbols", 1_000_000, &"word ".repeat(80));
        let large = estimate_from_text("code:symbols", 1_000_000, &"word ".repeat(800));
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
    fn search_documents_models_full_document_read_at_5x() {
        let s = estimate_from_text("memory:documents", 1_000_000, &"a".repeat(400));
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
        let s = estimate_from_text("code:files", 1_000_000, &"a".repeat(400));
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
        let s = estimate_from_text("code:outline", 0, &"x".repeat(800));
        assert_eq!(s.actual_tokens, 200);
        assert_eq!(s.baseline_tokens, 1_000);
        assert_eq!(s.est_tokens_saved, 800);
    }
}
