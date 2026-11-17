//! Bounding helpers for the commit lists of `git_log`, plus its single-commit (`rev`) form.
//!
//! A squash-merge commit can carry a message of hundreds of kilobytes (a pasted changelog, a list
//! of every squashed commit) and a file list in the thousands, so a list names each commit by its
//! summary alone and is held to a token budget as a whole. `show` is the escape hatch that returns
//! one commit whole.

use serde::Serialize;

use super::common::require_git_repo;
use crate::engine::budget::apply_budget;
use crate::engine::error::EngineError;
use crate::engine::types::git::{CommitFileView, GitCommitHit, GitShowResponse};
use crate::engine::workspace::Workspace;

/// Token budget applied to a commit list when the caller passes no `max_tokens`.
pub(super) const GIT_BUDGET_DEFAULT_TOKENS: u32 = 20_000;

/// Keep the leading commits that fit `max_tokens` (default [`GIT_BUDGET_DEFAULT_TOKENS`]).
///
/// Returns the kept commits and whether any were dropped; the caller then points `next_cursor` at
/// `skip + kept.len()` so the dropped tail is one more call away.
pub(super) fn budget_commits<T: Serialize>(commits: Vec<T>, max_tokens: Option<u32>) -> (Vec<T>, bool) {
    let budget = apply_budget(commits, Some(max_tokens.unwrap_or(GIT_BUDGET_DEFAULT_TOKENS)));
    (budget.items, budget.budgeted)
}

/// The `show` operation: one commit by sha or revision, with its whole message and its
/// changed files. The file list is held to `max_tokens`, and what it leaves out is counted.
pub(super) fn run_show(ws: &Workspace, sha: &str, max_tokens: Option<u32>) -> Result<GitShowResponse, EngineError> {
    let repo = require_git_repo(ws)?;
    let full = repo
        .resolve_rev(sha)
        .map_err(|e| EngineError::invalid_argument(format!("`git_log`: cannot resolve `{sha}`: {e}")))?;
    let info = repo
        .commit_record(&full)
        .ok_or_else(|| EngineError::invalid_argument(format!("`git_log`: `{sha}` is not a commit")))?;
    let files: Vec<CommitFileView> = info
        .files
        .iter()
        .map(|(path, kind)| CommitFileView {
            path: path.clone(),
            change: kind.as_str(),
        })
        .collect();
    let total = files.len();
    let files = apply_budget(files, max_tokens).items;
    Ok(GitShowResponse {
        commit: GitCommitHit {
            sha: info.sha,
            summary: info.summary,
            author: info.author,
            author_email: info.author_email,
            author_time_unix: info.author_time_unix,
            body: info.body,
        },
        budgeted: files.len() < total,
        omitted: total - files.len(),
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_keep_the_first_commit_even_when_it_alone_exceeds_the_budget() {
        let commits = vec!["x".repeat(4000), "y".repeat(4000)];
        let (kept, budgeted) = budget_commits(commits, Some(10));
        assert_eq!(kept.len(), 1);
        assert!(budgeted);
    }
}
