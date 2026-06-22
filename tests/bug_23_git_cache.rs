//! Regression: `cache stats` reported `git_cache_bytes: 0` even after git tool calls that
//! go through the disk-backed cache (bug #23). This drives a real disk-persisting
//! `GitCache` against a temp repo, exercises a cached + persisted category (`log`), and
//! asserts the on-disk size the stats path stats is non-zero — proving the cache is
//! disk-backed and the stat path matches the write dir.

use std::path::Path;
use std::process::Command;

use basemind::git::Repo;
use basemind::git_cache::GitCache;
use basemind::store_gc::cache_stats;

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

#[test]
fn git_cache_bytes_nonzero_after_disk_backed_log_call() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    run(root, &["init", "-q"]);
    run(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("a.txt"), b"hello\n").expect("write");
    run(root, &["add", "."]);
    run(root, &["commit", "-q", "-m", "init"]);

    let basemind_dir = root.join(".basemind");
    std::fs::create_dir_all(&basemind_dir).expect("mk basemind");

    let repo = Repo::discover(root).expect("discover");
    let head = repo.resolve_rev("HEAD").expect("resolve HEAD");

    // Disk-backed cache, as `serve` opens it by default.
    let cache = GitCache::open(&basemind_dir, 32, true).expect("open git cache");

    let before = cache_stats(&basemind_dir).expect("stats before");

    // A `recent_changes`-style call: persists a LogPayload under git-cache/log/.
    let commits = cache.log(&repo, &head, None, 50, true).expect("log");
    assert!(!commits.is_empty(), "repo has at least one commit");

    let after = cache_stats(&basemind_dir).expect("stats after");

    assert_eq!(before.git_cache_bytes, 0, "no bytes before any cached call");
    assert!(
        after.git_cache_bytes > 0,
        "git_cache_bytes must reflect the on-disk cache after a disk-backed call, got {}",
        after.git_cache_bytes
    );
}

#[test]
fn ram_only_git_cache_legitimately_persists_nothing() {
    // The companion truth for bug #23: with `persist=false` (`--no-git-cache-disk`) the
    // git cache is RAM-only by design, so `git_cache_bytes` staying 0 is honest, not a bug.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    run(root, &["init", "-q"]);
    run(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("a.txt"), b"hello\n").expect("write");
    run(root, &["add", "."]);
    run(root, &["commit", "-q", "-m", "init"]);

    let basemind_dir = root.join(".basemind");
    std::fs::create_dir_all(&basemind_dir).expect("mk basemind");

    let repo = Repo::discover(root).expect("discover");
    let head = repo.resolve_rev("HEAD").expect("resolve HEAD");

    let cache = GitCache::open(&basemind_dir, 32, false).expect("open ram-only git cache");
    let commits = cache.log(&repo, &head, None, 50, true).expect("log");
    assert!(!commits.is_empty(), "log returns commits from RAM");

    let stats = cache_stats(&basemind_dir).expect("stats");
    assert_eq!(
        stats.git_cache_bytes, 0,
        "RAM-only cache writes nothing to disk; 0 is the honest on-disk size"
    );
}
