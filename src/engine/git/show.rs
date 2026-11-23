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
use crate::engine::types::git::{CommitFileView, CommitView, FilesByCommit, FilesMore, GitCommitHit, GitShowResponse};
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

/// Changed files a commit lists in a recent or directory list before the rest are collapsed into
/// directories and counted; `git_log {rev}` lists them all.
pub(super) const FILES_PER_COMMIT: usize = 8;

/// Bytes a commit's `+N more` entry costs in the budget, beside its listed files.
const FILES_MORE_BYTES: u64 = 16;

/// `files` held to [`FILES_PER_COMMIT`] entries, and how many files that leaves out. A list longer
/// than the cap collapses each directory holding several of its files into one `dir/` entry saying
/// `"<n> files"`, in order of first appearance; the entries past the cap are counted, not listed.
pub(super) fn cap_files(files: Vec<CommitFileView>) -> (Vec<CommitFileView>, usize) {
    if files.len() <= FILES_PER_COMMIT {
        return (files, 0);
    }
    let mut groups: Vec<(crate::path::RelPath, Vec<CommitFileView>)> = Vec::new();
    for file in files {
        let bytes = file.path.as_bytes();
        let dir = bytes
            .iter()
            .rposition(|&b| b == b'/')
            .map_or(&bytes[..0], |at| &bytes[..=at]);
        let dir = crate::path::RelPath::from(dir);
        match groups.iter_mut().find(|(group, _)| *group == dir) {
            Some((_, members)) => members.push(file),
            None => groups.push((dir, vec![file])),
        }
    }
    let mut entries = groups.into_iter().map(|(dir, mut members)| {
        let count = members.len();
        match members.pop() {
            Some(only) if count == 1 => (only, 1),
            _ => (
                CommitFileView {
                    path: dir,
                    change: format!("{count} files").into(),
                },
                count,
            ),
        }
    });
    let kept: Vec<(CommitFileView, usize)> = entries.by_ref().take(FILES_PER_COMMIT).collect();
    let omitted = entries.map(|(_, count)| count).sum();
    (kept.into_iter().map(|(entry, _)| entry).collect(), omitted)
}

/// A commit list whose rows are written without their files, which go to the response's top-level
/// `files: {"<sha>": {"<path>": "<change>"}}` when the commits carry them: keep the leading commits
/// whose row and files fit `max_tokens` (default [`GIT_BUDGET_DEFAULT_TOKENS`]), and move each kept
/// commit's files out, held to [`FILES_PER_COMMIT`] with the rest counted in the returned
/// [`FilesMore`].
pub(super) fn budget_commits_with_files(
    commits: Vec<CommitView>,
    max_tokens: Option<u32>,
) -> (Vec<CommitView>, FilesByCommit, FilesMore, bool) {
    // Each list is capped before the budget is spent: a commit changing thousands of files is
    // charged for the entries the response carries, not for every path.
    let rows: Vec<(CommitView, Option<(Vec<CommitFileView>, usize)>)> = commits
        .into_iter()
        .map(|mut commit| {
            let capped = commit.files.take().map(cap_files);
            (commit, capped)
        })
        .collect();
    let budget = keep_prefix(
        rows,
        Some(max_tokens.unwrap_or(GIT_BUDGET_DEFAULT_TOKENS)),
        |(commit, capped)| {
            let files = capped.as_ref().map_or(0, |(listed, omitted)| {
                let more = if *omitted > 0 { FILES_MORE_BYTES } else { 0 };
                listed
                    .iter()
                    .map(|file| file.path.as_bytes().len() as u64 + 16)
                    .sum::<u64>()
                    + more
            });
            bytes_to_tokens(commit.sha.len() as u64 + commit.summary.len() as u64 + 8 + files).max(1)
        },
    );
    let budgeted = budget.budgeted;
    let mut commits = Vec::with_capacity(budget.items.len());
    let mut files = FilesByCommit::new();
    let mut more = FilesMore::new();
    for (commit, capped) in budget.items {
        if let Some((listed, omitted)) = capped {
            if omitted > 0 {
                more.push((commit.sha.clone(), omitted));
            }
            files.push((commit.sha.clone(), listed));
        }
        commits.push(commit);
    }
    (commits, files, more, budgeted)
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
            change: kind.as_str().into(),
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
            author_time_unix: 0,
            files: Some(
                files
                    .iter()
                    .map(|path| CommitFileView {
                        path: crate::path::RelPath::from(path.as_bytes()),
                        change: "modified".into(),
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
        let (kept, files, _, budgeted) = budget_commits_with_files(commits, Some(20));
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

        let (kept, files, _, budgeted) =
            budget_commits_with_files(vec![commit("a", &["1", "2", "3", "4"]), commit("b", &[])], Some(21));
        assert_eq!((kept.len(), files.len(), budgeted), (2, 2, false));
    }

    fn changed(paths: &[&str]) -> Vec<CommitFileView> {
        paths
            .iter()
            .map(|path| CommitFileView {
                path: crate::path::RelPath::from(path.as_bytes()),
                change: "modified".into(),
            })
            .collect()
    }

    fn listed(files: &[CommitFileView]) -> Vec<(String, String)> {
        files
            .iter()
            .map(|file| (file.path.to_string(), file.change.to_string()))
            .collect()
    }

    #[test]
    fn a_list_within_the_cap_is_kept_whole() {
        let paths: Vec<String> = (0..FILES_PER_COMMIT).map(|n| format!("d/f{n}")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let (kept, omitted) = cap_files(changed(&refs));
        assert_eq!((kept.len(), omitted), (FILES_PER_COMMIT, 0));
        assert_eq!(listed(&kept)[0], ("d/f0".to_string(), "modified".to_string()));
    }

    #[test]
    fn a_long_list_collapses_directories_and_counts_what_the_cap_leaves_out() {
        let mut paths: Vec<String> = (0..6).map(|n| format!("big/f{n}")).collect();
        paths.extend((0..FILES_PER_COMMIT).map(|n| format!("d{n}/only.rs")));
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let (kept, omitted) = cap_files(changed(&refs));
        let listed = listed(&kept);
        assert_eq!(listed.len(), FILES_PER_COMMIT);
        assert_eq!(listed[0], ("big/".to_string(), "6 files".to_string()));
        assert_eq!(listed[1], ("d0/only.rs".to_string(), "modified".to_string()));
        assert_eq!(listed[7], ("d6/only.rs".to_string(), "modified".to_string()));
        assert_eq!(omitted, 1, "the one single-file group past the cap");
    }

    /// The budget is spent on the capped list, so a commit changing thousands of files under one
    /// directory does not push the commits after it off the page.
    #[test]
    fn a_huge_file_list_is_charged_as_capped() {
        let paths: Vec<String> = (0..2_000).map(|n| format!("big/f{n}")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let mut huge = commit("a", &[]);
        huge.files = Some(changed(&refs));
        let (kept, files, more, budgeted) = budget_commits_with_files(vec![huge, commit("b", &["1"])], Some(20));
        assert_eq!((kept.len(), budgeted), (2, false));
        assert_eq!(
            listed(&files[0].1),
            vec![("big/".to_string(), "2000 files".to_string())]
        );
        assert!(more.is_empty());
    }

    #[test]
    fn a_commit_past_the_cap_reports_its_omitted_files_and_one_without_files_is_not_listed() {
        let paths: Vec<String> = (0..12).map(|n| format!("d{n}/f.rs")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let mut bare = commit("b", &[]);
        bare.files = None;
        let mut wide = commit("a", &[]);
        wide.files = Some(changed(&refs));
        let (kept, files, more, budgeted) = budget_commits_with_files(vec![wide, bare], None);
        assert_eq!((kept.len(), budgeted), (2, false));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1.len(), FILES_PER_COMMIT);
        assert_eq!(more, vec![("a".to_string(), 4)]);
    }
}
