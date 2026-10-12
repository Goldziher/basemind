//! Bounding helpers for the commit-list modes of the `git` tool, plus `git` mode `show`.
//!
//! A squash-merge commit can carry a message of hundreds of kilobytes (a pasted changelog, a list
//! of every squashed commit) and a file list in the thousands, so a list response is bounded two
//! ways: each hit's body is cut at `body_chars`, and the commit list as a whole is held to a token
//! budget. `show` is the escape hatch that returns one commit whole.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde::Serialize;

use super::ServerState;
use super::budget::apply_budget;
use super::helpers::{elapsed_us, json_result, require_git_repo};
use super::types::{CommitFileView, GitCommitHit, GitShowResponse};
use crate::git::CommitInfo;

/// Token budget applied to a commit list when the caller passes no `max_tokens`.
pub(super) const GIT_BUDGET_DEFAULT_TOKENS: u32 = 20_000;

/// Characters of commit body kept per `search` hit when the caller passes no `body_chars`.
pub(super) const BODY_CHARS_DEFAULT: usize = 1_000;

/// Upper bound on `body_chars`; a caller wanting more uses `show`.
pub(super) const BODY_CHARS_MAX: usize = 20_000;

/// Keep the leading commits that fit `max_tokens` (default [`GIT_BUDGET_DEFAULT_TOKENS`]).
///
/// Returns the kept commits and whether any were dropped; the caller then points `next_cursor` at
/// `skip + kept.len()` so the dropped tail is one more call away.
pub(super) fn budget_commits<T: Serialize>(commits: Vec<T>, max_tokens: Option<u32>) -> (Vec<T>, bool) {
    let budget = apply_budget(commits, Some(max_tokens.unwrap_or(GIT_BUDGET_DEFAULT_TOKENS)));
    (budget.items, budget.budgeted)
}

/// Cut `body` to `max_chars` characters. Returns the (possibly cut) body, whether it was cut, and
/// the full length in bytes when it was. A cut body ends in a marker that names the way to the
/// whole message, so a reader of the raw text is not left thinking the message just stops.
pub(super) fn cap_body(body: String, max_chars: usize, short_sha: &str) -> (String, bool, Option<usize>) {
    let Some((cut, _)) = body.char_indices().nth(max_chars) else {
        return (body, false, None);
    };
    let total = body.len();
    if max_chars == 0 {
        return (String::new(), true, Some(total));
    }
    let mut kept = body;
    kept.truncate(cut);
    kept.push_str(&format!(
        "\n[... body truncated, {total} bytes in all; `git` mode=\"show\" sha=\"{short_sha}\" returns it whole]"
    ));
    (kept, true, Some(total))
}

/// Build a search hit, capping its body at `body_chars`.
pub(super) fn commit_to_hit(c: CommitInfo, body_chars: usize) -> GitCommitHit {
    let (body, body_truncated, body_bytes) = cap_body(c.body, body_chars, &c.short_sha);
    GitCommitHit {
        sha: c.sha,
        short_sha: c.short_sha,
        summary: c.summary,
        author: c.author,
        author_email: c.author_email,
        author_time_unix: c.author_time_unix,
        body,
        body_truncated,
        body_bytes,
    }
}

/// The `body_chars` a caller asked for, clamped to [`BODY_CHARS_MAX`].
pub(super) fn resolve_body_chars(requested: Option<u32>) -> usize {
    requested.map_or(BODY_CHARS_DEFAULT, |n| (n as usize).min(BODY_CHARS_MAX))
}

/// Body for `git` mode `show`: one commit by sha or revision, with its whole message and its
/// changed files.
pub(super) fn run_show(state: &ServerState, sha: &str) -> Result<CallToolResult, McpError> {
    let started = std::time::Instant::now();
    let repo = require_git_repo(state)?;
    let full = repo
        .resolve_rev(sha)
        .map_err(|e| McpError::invalid_params(format!("`git` mode=\"show\": cannot resolve `{sha}`: {e}"), None))?;
    let info = repo
        .commit_record(&full)
        .ok_or_else(|| McpError::invalid_params(format!("`git` mode=\"show\": `{sha}` is not a commit"), None))?;
    let files = info
        .files
        .iter()
        .map(|(path, kind)| CommitFileView {
            path: path.clone(),
            change: kind.as_str(),
        })
        .collect();
    json_result(&GitShowResponse {
        commit: commit_to_hit(info, usize::MAX),
        files,
        elapsed_us: elapsed_us(started),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_leave_a_short_body_untouched() {
        let (body, cut, bytes) = cap_body("short".to_string(), 1000, "abc1234");
        assert_eq!((body.as_str(), cut, bytes), ("short", false, None));
    }

    #[test]
    fn should_cut_a_long_body_at_a_char_boundary_and_append_the_marker() {
        let long = "é".repeat(50);
        let (body, cut, bytes) = cap_body(long.clone(), 10, "abc1234");
        assert!(cut);
        assert_eq!(bytes, Some(long.len()));
        assert!(body.starts_with(&"é".repeat(10)));
        assert!(body.contains("mode=\"show\" sha=\"abc1234\""), "{body}");
        assert!(!body.contains(&"é".repeat(11)));
    }

    #[test]
    fn should_drop_the_body_entirely_when_zero_chars_are_allowed() {
        let (body, cut, bytes) = cap_body("something".to_string(), 0, "abc1234");
        assert_eq!((body.as_str(), cut, bytes), ("", true, Some(9)));
    }

    #[test]
    fn should_treat_a_body_of_exactly_the_cap_as_whole() {
        let (body, cut, _) = cap_body("x".repeat(10), 10, "abc1234");
        assert!(!cut);
        assert_eq!(body.len(), 10);
    }

    #[test]
    fn should_clamp_requested_body_chars() {
        assert_eq!(resolve_body_chars(None), BODY_CHARS_DEFAULT);
        assert_eq!(resolve_body_chars(Some(5)), 5);
        assert_eq!(resolve_body_chars(Some(u32::MAX)), BODY_CHARS_MAX);
    }

    #[test]
    fn should_keep_the_first_commit_even_when_it_alone_exceeds_the_budget() {
        let commits = vec!["x".repeat(4000), "y".repeat(4000)];
        let (kept, budgeted) = budget_commits(commits, Some(10));
        assert_eq!(kept.len(), 1);
        assert!(budgeted);
    }
}
