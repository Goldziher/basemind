//! The working-tree status is cached per worktree (#284): a repeat call with nothing changed runs no
//! status, and the index, `HEAD` and the watcher's invalidation each make the next call compute.

use std::fs;
use std::path::Path;

use basemind::git::Repo;
use tempfile::TempDir;

fn run(repo: &Path, args: &[&str]) {
    let status = basemind::testing::git_command()
        .args(args)
        .current_dir(repo)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

/// A repository with one committed file, `tracked.txt`.
fn committed_repo() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    run(dir.path(), &["init", "-q"]);
    run(dir.path(), &["config", "commit.gpgsign", "false"]);
    fs::write(dir.path().join("tracked.txt"), b"original\n").unwrap();
    run(dir.path(), &["add", "tracked.txt"]);
    run(dir.path(), &["commit", "-q", "-m", "base"]);
    dir
}

fn untracked(repo: &Repo) -> Vec<String> {
    repo.status_per_file()
        .expect("status")
        .untracked
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn consecutive_statuses_with_no_change_run_one_computation() {
    let dir = committed_repo();
    let repo = Repo::discover(dir.path()).expect("discover");

    let first = repo.status_per_file().expect("status");
    let second = repo.status_per_file().expect("status");

    assert_eq!(repo.status_computations(), 1);
    assert_eq!(first.untracked, second.untracked);
    assert!(first.modified.is_empty() && first.untracked.is_empty());
}

#[test]
fn each_listing_is_cached_on_its_own() {
    let dir = committed_repo();
    let repo = Repo::discover(dir.path()).expect("discover");

    repo.status_per_file().expect("status");
    repo.status_porcelain().expect("status");
    repo.status_per_file().expect("status");
    repo.status_porcelain().expect("status");

    assert_eq!(repo.status_computations(), 2);
}

#[test]
fn an_invalidation_from_the_watcher_makes_the_next_status_recompute() {
    let dir = committed_repo();
    let repo = Repo::discover(dir.path()).expect("discover");
    assert!(untracked(&repo).is_empty());

    fs::write(dir.path().join("new.txt"), b"new\n").unwrap();
    repo.invalidate_status();

    assert_eq!(untracked(&repo), vec!["new.txt".to_string()]);
    assert_eq!(repo.status_computations(), 2);
}

#[test]
fn an_edit_with_no_invalidation_is_not_seen_until_the_cache_is_stale() {
    let dir = committed_repo();
    let repo = Repo::discover(dir.path()).expect("discover");
    assert!(untracked(&repo).is_empty());

    fs::write(dir.path().join("new.txt"), b"new\n").unwrap();

    assert!(untracked(&repo).is_empty());
    assert_eq!(repo.status_computations(), 1);
}

#[test]
fn staging_changes_the_index_and_makes_the_next_status_recompute() {
    let dir = committed_repo();
    let repo = Repo::discover(dir.path()).expect("discover");
    fs::write(dir.path().join("new.txt"), b"new\n").unwrap();
    repo.invalidate_status();
    assert_eq!(untracked(&repo), vec!["new.txt".to_string()]);

    run(dir.path(), &["add", "new.txt"]);
    let status = repo.status_per_file().expect("status");

    assert_eq!(repo.status_computations(), 2);
    assert_eq!(status.staged_added.len(), 1);
    assert!(status.untracked.is_empty());
}

#[test]
fn a_commit_moves_head_and_makes_the_next_status_recompute() {
    let dir = committed_repo();
    let repo = Repo::discover(dir.path()).expect("discover");
    fs::write(dir.path().join("tracked.txt"), b"edited\n").unwrap();
    repo.invalidate_status();
    assert_eq!(repo.status_per_file().expect("status").modified.len(), 1);

    run(dir.path(), &["commit", "-q", "-a", "-m", "edit"]);
    let status = repo.status_per_file().expect("status");

    assert_eq!(repo.status_computations(), 2);
    assert!(status.modified.is_empty());
}
