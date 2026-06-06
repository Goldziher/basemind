//! Integration tests for the sha-keyed git cache.

use std::fs;
use std::path::Path;
use std::process::Command;

use gitmind::git::Repo;
use gitmind::git_cache::{GIT_CACHE_DIR, GitCache};
use tempfile::TempDir;

fn run(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

fn three_commit_repo() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run(root, &["init", "-q"]);
    run(root, &["config", "commit.gpgsign", "false"]);
    for i in 0..3 {
        fs::write(root.join("a.rs"), format!("pub fn v{i}() {{}}\n")).unwrap();
        run(root, &["add", "a.rs"]);
        run(root, &["commit", "-qm", &format!("rev {i}")]);
    }
    dir
}

#[test]
fn commit_files_cache_round_trip() {
    let dir = three_commit_repo();
    let root = dir.path();
    let gitmind_dir = root.join(".gitmind");
    fs::create_dir_all(&gitmind_dir).unwrap();
    let cache = GitCache::open(&gitmind_dir, 8, true).unwrap();
    let repo = Repo::discover(root).unwrap();
    let head = repo.resolve_rev("HEAD").unwrap();

    // First call: cold — populates RAM and disk.
    let first = cache.commit_files(&repo, &head).unwrap();
    assert!(!first.is_empty(), "expected commit_files to be non-empty");

    // Disk artifact exists at the sha-keyed path.
    let on_disk = gitmind_dir
        .join(GIT_CACHE_DIR)
        .join("commit_files")
        .join(format!("{head}.msgpack"));
    assert!(on_disk.exists(), "commit_files disk entry missing");

    // RAM hit: same Arc identity.
    let second = cache.commit_files(&repo, &head).unwrap();
    assert!(
        std::sync::Arc::ptr_eq(&first, &second),
        "RAM hit must return the same Arc"
    );
}

#[test]
fn log_cache_round_trip() {
    let dir = three_commit_repo();
    let root = dir.path();
    let gitmind_dir = root.join(".gitmind");
    fs::create_dir_all(&gitmind_dir).unwrap();
    let cache = GitCache::open(&gitmind_dir, 8, true).unwrap();
    let repo = Repo::discover(root).unwrap();
    let head = repo.resolve_rev("HEAD").unwrap();

    let first = cache.log(&repo, &head, None, 10, true).unwrap();
    assert_eq!(first.len(), 3, "three commits expected");

    let second = cache.log(&repo, &head, None, 10, true).unwrap();
    assert!(
        std::sync::Arc::ptr_eq(&first, &second),
        "second log call should hit RAM"
    );

    // Disk persists.
    let log_dir = gitmind_dir.join(GIT_CACHE_DIR).join("log");
    let entries: Vec<_> = fs::read_dir(&log_dir).unwrap().flatten().collect();
    assert_eq!(entries.len(), 1, "exactly one log disk entry expected");
}

#[test]
fn disk_persistence_survives_reopen() {
    let dir = three_commit_repo();
    let root = dir.path();
    let gitmind_dir = root.join(".gitmind");
    fs::create_dir_all(&gitmind_dir).unwrap();
    let repo = Repo::discover(root).unwrap();
    let head = repo.resolve_rev("HEAD").unwrap();

    {
        let cache = GitCache::open(&gitmind_dir, 8, true).unwrap();
        cache.commit_files(&repo, &head).unwrap();
    }
    // New cache instance — RAM is empty, but disk should still hit.
    let cache = GitCache::open(&gitmind_dir, 8, true).unwrap();
    let arc = cache.commit_files(&repo, &head).unwrap();
    assert!(!arc.is_empty(), "second cache should populate from disk");
}

#[test]
fn clear_removes_disk_files() {
    let dir = three_commit_repo();
    let root = dir.path();
    let gitmind_dir = root.join(".gitmind");
    fs::create_dir_all(&gitmind_dir).unwrap();
    let cache = GitCache::open(&gitmind_dir, 8, true).unwrap();
    let repo = Repo::discover(root).unwrap();
    let head = repo.resolve_rev("HEAD").unwrap();
    cache.commit_files(&repo, &head).unwrap();
    cache.log(&repo, &head, None, 10, true).unwrap();

    let removed = cache.clear().unwrap();
    assert!(
        removed >= 2,
        "expected at least the two seeded entries; got {removed}"
    );

    // Recompute hits cold path now.
    cache.commit_files(&repo, &head).unwrap();
}

#[test]
fn ram_only_mode_skips_disk_writes() {
    let dir = three_commit_repo();
    let root = dir.path();
    let gitmind_dir = root.join(".gitmind");
    fs::create_dir_all(&gitmind_dir).unwrap();
    let cache = GitCache::open(&gitmind_dir, 8, false).unwrap();
    let repo = Repo::discover(root).unwrap();
    let head = repo.resolve_rev("HEAD").unwrap();
    cache.commit_files(&repo, &head).unwrap();

    let on_disk = gitmind_dir.join(GIT_CACHE_DIR);
    assert!(
        !on_disk.exists(),
        "persist=false must not create the cache dir"
    );
}
