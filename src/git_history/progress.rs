//! Progress of a from-scratch git-history build handed to the background worker.
//!
//! A one-shot `basemind scan` that finds the index owing a full rebuild leaves it to the detached
//! `embed-worker` and writes `git-history-progress.json` beside the index: the commits the build
//! walks, none of them stored yet. The worker rewrites it before each chunk of commits it folds,
//! and any sync that succeeds removes it ([`super::builder::sync_with`]). `index_status` reports it
//! as `git_history_build`. The record has the shape of [`crate::embed_progress`]'s, counting
//! commits, and carries the writer's identity the same way, so a build in progress is told from
//! one that was killed.
//!
//! The record is also the worker's order: the worker builds the index only while one exists, so a
//! scan run with `--no-git-history` hands it nothing.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::embed_progress::{counts, read_record, record_of, write_record};

/// Filename of the record in the git-history cache dir.
const PROGRESS_FILE: &str = "git-history-progress.json";

/// The `git_history_build` block of `index_status`: how far the background build has got.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct HistoryBuildStatus {
    /// Commits stored so far.
    pub done: usize,
    /// Commits the build stores, including those done.
    pub total: usize,
    /// Seconds until the build finishes at the rate it has run so far. Absent while no build runs
    /// (interrupted, or not yet started) and before its first chunk of commits is stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<u64>,
}

fn progress_path(basemind_dir: &Path) -> PathBuf {
    basemind_dir.join(PROGRESS_FILE)
}

/// Note that a build of `commits` commits is owed to the background worker, with none stored yet.
pub fn record_owed(basemind_dir: &Path, commits: usize) {
    write_record(&progress_path(basemind_dir), &record_of(commits, 0, None));
}

/// Whether a build is owed to the background worker.
pub fn is_owed(basemind_dir: &Path) -> bool {
    read_record(&progress_path(basemind_dir)).is_some()
}

/// Remove the record: the index owes the background nothing.
pub fn clear(basemind_dir: &Path) {
    let _ = std::fs::remove_file(progress_path(basemind_dir));
}

/// Record that the build begun at `started_unix_ms` has stored `done` of its `total` commits.
pub(crate) fn report(basemind_dir: &Path, started_unix_ms: u64, done: usize, total: usize) {
    write_record(
        &progress_path(basemind_dir),
        &record_of(total.max(done), done, Some(started_unix_ms)),
    );
}

/// The build's progress at `now_unix_ms`, or `None` when no build is owed.
pub fn status(basemind_dir: &Path, now_unix_ms: u64) -> Option<HistoryBuildStatus> {
    let (done, total, eta_secs) = counts(&read_record(&progress_path(basemind_dir))?, now_unix_ms)?;
    Some(HistoryBuildStatus { done, total, eta_secs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owed_build_is_reported_until_it_is_cleared() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!is_owed(dir.path()));
        assert_eq!(status(dir.path(), 0), None);

        record_owed(dir.path(), 250);
        assert!(is_owed(dir.path()));
        assert_eq!(
            status(dir.path(), 0),
            Some(HistoryBuildStatus {
                done: 0,
                total: 250,
                eta_secs: None
            })
        );

        clear(dir.path());
        assert!(!is_owed(dir.path()));
        assert_eq!(status(dir.path(), 0), None);
    }

    #[test]
    fn a_running_build_reports_the_time_it_has_left_at_its_own_rate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cases = [
            (0, 100, None),
            (25, 100, Some(30)),
            (50, 100, Some(10)),
            (100, 100, None),
        ];
        for (done, total, eta_secs) in cases {
            report(dir.path(), 1_000, done, total);
            assert_eq!(
                status(dir.path(), 11_000),
                Some(HistoryBuildStatus { done, total, eta_secs }),
                "{done} of {total} in 10 s"
            );
        }
    }

    #[test]
    fn a_count_past_the_total_raises_the_total() {
        let dir = tempfile::tempdir().expect("tempdir");
        report(dir.path(), 1_000, 120, 100);
        assert_eq!(
            status(dir.path(), 11_000).map(|status| (status.done, status.total)),
            Some((120, 120))
        );
    }
}
