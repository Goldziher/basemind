//! Integration tests for the sha-keyed git cache.

/// The git-cache schema is unified with `RELEASE_MINOR` so every on-disk cache invalidates
/// in lock-step on a minor-release bump. Mirror the exact derivation here so a future change
/// to the formula (or an accidental revert to the old hardcoded `1`) fails loudly.
#[test]
fn git_cache_schema_tracks_release_minor() {
    assert_eq!(
        GIT_CACHE_SCHEMA,
        RELEASE_MINOR + 1,
        "GIT_CACHE_SCHEMA must derive from RELEASE_MINOR (the +1 offset), not a hardcoded value"
    );
    assert_ne!(
        GIT_CACHE_SCHEMA, 1,
        "must differ from the historical hardcoded 1 so the next release invalidates stale payloads"
    );
}

use std::fs;
use std::path::Path;
use std::process::Command;

use basemind::git::Repo;
use basemind::git_cache::{GIT_CACHE_DIR, GIT_CACHE_SCHEMA, GitCache};
use basemind::version::RELEASE_MINOR;
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
    let basemind_dir = root.join(".basemind");
    fs::create_dir_all(&basemind_dir).unwrap();
    let cache = GitCache::open(&basemind_dir, 8, true).unwrap();
    let repo = Repo::discover(root).unwrap();
    let head = repo.resolve_rev("HEAD").unwrap();

    // First call: cold — populates RAM and disk.
    let first = cache.commit_files(&repo, &head).unwrap();
    assert!(!first.is_empty(), "expected commit_files to be non-empty");

    // Disk artifact exists at the sha-keyed path.
    let on_disk = basemind_dir
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
    let basemind_dir = root.join(".basemind");
    fs::create_dir_all(&basemind_dir).unwrap();
    let cache = GitCache::open(&basemind_dir, 8, true).unwrap();
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
    let log_dir = basemind_dir.join(GIT_CACHE_DIR).join("log");
    let entries: Vec<_> = fs::read_dir(&log_dir).unwrap().flatten().collect();
    assert_eq!(entries.len(), 1, "exactly one log disk entry expected");
}

#[test]
fn disk_persistence_survives_reopen() {
    let dir = three_commit_repo();
    let root = dir.path();
    let basemind_dir = root.join(".basemind");
    fs::create_dir_all(&basemind_dir).unwrap();
    let repo = Repo::discover(root).unwrap();
    let head = repo.resolve_rev("HEAD").unwrap();

    {
        let cache = GitCache::open(&basemind_dir, 8, true).unwrap();
        cache.commit_files(&repo, &head).unwrap();
    }
    // New cache instance — RAM is empty, but disk should still hit.
    let cache = GitCache::open(&basemind_dir, 8, true).unwrap();
    let arc = cache.commit_files(&repo, &head).unwrap();
    assert!(!arc.is_empty(), "second cache should populate from disk");
}

#[test]
fn clear_removes_disk_files() {
    let dir = three_commit_repo();
    let root = dir.path();
    let basemind_dir = root.join(".basemind");
    fs::create_dir_all(&basemind_dir).unwrap();
    let cache = GitCache::open(&basemind_dir, 8, true).unwrap();
    let repo = Repo::discover(root).unwrap();
    let head = repo.resolve_rev("HEAD").unwrap();
    cache.commit_files(&repo, &head).unwrap();
    cache.log(&repo, &head, None, 10, true).unwrap();

    let removed = cache.clear().unwrap();
    assert!(removed >= 2, "expected at least the two seeded entries; got {removed}");

    // Recompute hits cold path now.
    cache.commit_files(&repo, &head).unwrap();
}

#[test]
fn ram_only_mode_skips_disk_writes() {
    let dir = three_commit_repo();
    let root = dir.path();
    let basemind_dir = root.join(".basemind");
    fs::create_dir_all(&basemind_dir).unwrap();
    let cache = GitCache::open(&basemind_dir, 8, false).unwrap();
    let repo = Repo::discover(root).unwrap();
    let head = repo.resolve_rev("HEAD").unwrap();
    cache.commit_files(&repo, &head).unwrap();

    let on_disk = basemind_dir.join(GIT_CACHE_DIR);
    assert!(!on_disk.exists(), "persist=false must not create the cache dir");
}
