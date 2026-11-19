//! Bounding helpers for the commit lists of `git_log`, plus its single-commit (`rev`) form.
//!
//! A squash-merge commit can carry a message of hundreds of kilobytes (a pasted changelog, a list
//! of every squashed commit) and a file list in the thousands, so a list names each commit by its
//! summary alone and is held to a token budget as a whole. `show` is the escape hatch that returns
//! one commit whole.

use serde::Serialize;

use super::common::require_git_repo;
use crate::engine::budget::{apply_budget, keep_prefix};
use crate::engine::error::EngineError;
use crate::engine::tokens::bytes_to_tokens;
use crate::engine::types::git::{CommitFileView, CommitView, FilesByCommit, GitCommitHit, GitShowResponse};
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

/// A commit list whose rows are written without their files, which go to the response's top-level
/// `files: {"<sha>": {"<path>": "<change>"}}`: keep the leading commits whose row and files fit
/// `max_tokens` (default [`GIT_BUDGET_DEFAULT_TOKENS`]), and move each kept commit's files out.
pub(super) fn budget_commits_with_files(
    commits: Vec<CommitView>,
    max_tokens: Option<u32>,
) -> (Vec<CommitView>, FilesByCommit, bool) {
    let budget = keep_prefix(
        commits,
        Some(max_tokens.unwrap_or(GIT_BUDGET_DEFAULT_TOKENS)),
        |commit| {
            let files = commit.files.as_deref().map_or(0, |files| {
                files
                    .iter()
                    .map(|file| file.path.as_bytes().len() as u64 + 16)
                    .sum::<u64>()
            });
            bytes_to_tokens(commit.sha.len() as u64 + commit.summary.len() as u64 + 8 + files).max(1)
        },
    );
    let (mut commits, budgeted) = (budget.items, budget.budgeted);
    let files = commits
        .iter_mut()
        .map(|commit| (commit.sha.clone(), commit.files.take().unwrap_or_default()))
        .collect();
    (commits, files, budgeted)
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

    fn commit(sha: &str, files: &[&str]) -> CommitView {
        CommitView {
            sha: sha.into(),
            summary: "s".into(),
            author: "a".into(),
            with_author: false,
            author_time_unix: 0,
            files: Some(
                files
                    .iter()
                    .map(|path| CommitFileView {
                        path: crate::path::RelPath::from(path.as_bytes()),
                        change: "modified",
                    })
                    .collect(),
            ),
        }
    }

    /// A commit's files are written in the response's `files`, so they are charged to it, and the
    /// kept commits hand their files over to that map.
    #[test]
    fn a_commit_is_charged_its_files_and_hands_them_to_the_files_map() {
        // A 1-byte path costs 17 bytes; sha + summary + 8 is 11: four files make a commit 79 bytes
        // (19 tokens), a commit without files 11 bytes (2 tokens). Without the files' cost the pair
        // would be 4 tokens and fit either budget.
        let commits = vec![commit("a", &["1", "2", "3", "4"]), commit("b", &[])];
        let (kept, files, budgeted) = budget_commits_with_files(commits, Some(20));
        assert_eq!(
            kept.len(),
            1,
            "the second commit no longer fits once the first pays its files"
        );
        assert!(budgeted);
        assert!(kept[0].files.is_none(), "a kept commit's files moved out of the row");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "a");
        assert_eq!(files[0].1.len(), 4);

        let (kept, files, budgeted) =
            budget_commits_with_files(vec![commit("a", &["1", "2", "3", "4"]), commit("b", &[])], Some(21));
        assert_eq!((kept.len(), files.len(), budgeted), (2, 2, false));
    }
}
