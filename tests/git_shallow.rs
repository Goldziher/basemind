//! Shallow clones over real `git clone --depth` fixtures: what is cached while a clone is shallow,
//! and what changes once it is unshallowed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use basemind::git::{BlameResult, ChangeKind, Repo};
use basemind::git_cache::{GIT_CACHE_DIR, GitCache};
use basemind::git_history::GitHistoryIndex;
use basemind::git_history::builder::{self, RebuildOutcome};
use basemind::path::RelPath;
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

/// An upstream whose `a.rs` gains one line per commit (`line 0`, `line 1`, `line 2`), its commit
/// shas oldest first, and the path of a `--depth 1` clone of it.
fn shallow_clone_of_three_commits() -> (TempDir, Vec<String>, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let upstream = dir.path().join("upstream");
    fs::create_dir_all(&upstream).unwrap();
    run(&upstream, &["init", "-q"]);
    run(&upstream, &["config", "commit.gpgsign", "false"]);
    let mut content = String::new();
    for i in 0..3 {
        content.push_str(&format!("line {i}\n"));
        fs::write(upstream.join("a.rs"), &content).unwrap();
        run(&upstream, &["add", "a.rs"]);
        run(&upstream, &["commit", "-qm", &format!("rev {i}")]);
    }
    let upstream_repo = Repo::discover(&upstream).unwrap();
    let oldest_first = ["HEAD~2", "HEAD~1", "HEAD"]
        .iter()
        .map(|rev| upstream_repo.resolve_rev(rev).unwrap())
        .collect();
    let url = format!("file://{}", upstream.display());
    run(dir.path(), &["clone", "-q", "--depth", "1", &url, "clone"]);
    let clone = dir.path().join("clone");
    (dir, oldest_first, clone)
}

/// Each blame hunk as its first line, its length and the commit it names.
fn blamed_lines(result: &BlameResult) -> Vec<(u32, u32, String)> {
    result
        .hunks
        .iter()
        .map(|hunk| (hunk.start_line, hunk.len, hunk.commit_sha.clone()))
        .collect()
}

fn cached_files(basemind_dir: &Path, sub: &str) -> usize {
    fs::read_dir(basemind_dir.join(GIT_CACHE_DIR).join(sub))
        .unwrap()
        .count()
}

#[test]
fn answers_computed_while_shallow_are_recomputed_once_the_clone_is_unshallowed() {
    let (_dir, oldest_first, clone) = shallow_clone_of_three_commits();
    let basemind_dir = clone.join(".basemind");
    fs::create_dir_all(&basemind_dir).unwrap();
    let cache = GitCache::open(&basemind_dir, 8, true).unwrap();
    let repo = Repo::discover(&clone).unwrap();
    let head = repo.resolve_rev("HEAD").unwrap();
    let path = RelPath::from("a.rs");
    assert_eq!(head, oldest_first[2]);
    assert!(repo.is_shallow());

    let full_blame: Vec<(u32, u32, String)> = oldest_first
        .iter()
        .zip(1..)
        .map(|(sha, line)| (line, 1, sha.clone()))
        .collect();
    let shallow_blame = cache.blame(&repo, &head, &path, None).unwrap();
    assert_ne!(blamed_lines(&shallow_blame), full_blame);
    assert!(
        shallow_blame.hunks.iter().all(|hunk| hunk.commit_sha == head),
        "a depth-1 blame names only the boundary commit: {shallow_blame:?}"
    );
    let shallow_log: Vec<String> = cache
        .log(&repo, &head, None, 10, false)
        .unwrap()
        .iter()
        .map(|commit| commit.sha.clone())
        .collect();
    assert_eq!(shallow_log, [head.clone()]);
    cache.commit_files(&repo, &head).unwrap();
    for sub in ["blame", "log", "commit_files"] {
        assert_eq!(cached_files(&basemind_dir, sub), 0, "{sub} cached while shallow");
    }

    run(&clone, &["fetch", "-q", "--unshallow"]);
    assert!(!repo.is_shallow(), "the same Repo sees the clone unshallowed");

    let unshallowed = cache.blame(&repo, &head, &path, None).unwrap();
    assert_eq!(blamed_lines(&unshallowed), full_blame);
    assert_eq!(unshallowed.truncated_reason, None);
    let mut full_log: Vec<String> = cache
        .log(&repo, &head, None, 10, false)
        .unwrap()
        .iter()
        .map(|commit| commit.sha.clone())
        .collect();
    full_log.sort();
    let mut expected_log = oldest_first.clone();
    expected_log.sort();
    assert_eq!(full_log, expected_log);
    assert_eq!(
        *cache.commit_files(&repo, &head).unwrap(),
        vec![(RelPath::from("a.rs"), ChangeKind::Modified)]
    );
    for sub in ["blame", "log", "commit_files"] {
        assert_eq!(cached_files(&basemind_dir, sub), 1, "{sub} cached once unshallowed");
    }
}

#[test]
fn a_history_index_built_while_shallow_is_rebuilt_once_the_clone_is_unshallowed() {
    let (_dir, _, clone) = shallow_clone_of_three_commits();
    let basemind_dir = clone.join(".basemind");
    fs::create_dir_all(&basemind_dir).unwrap();
    let index = GitHistoryIndex::open(&basemind_dir).unwrap();
    let repo = Repo::discover(&clone).unwrap();

    assert_eq!(
        builder::sync(&index, &repo, &basemind_dir).unwrap(),
        RebuildOutcome::FullRebuild {
            reason: "initial",
            commits: 1
        }
    );
    assert_eq!(
        builder::sync(&index, &repo, &basemind_dir).unwrap(),
        RebuildOutcome::Fresh
    );

    run(&clone, &["fetch", "-q", "--unshallow"]);
    assert_eq!(
        builder::sync(&index, &repo, &basemind_dir).unwrap(),
        RebuildOutcome::FullRebuild {
            reason: "history-rewrite",
            commits: 3
        }
    );
    assert_eq!(index.commit_count(), 3);
    assert_eq!(
        builder::sync(&index, &repo, &basemind_dir).unwrap(),
        RebuildOutcome::Fresh
    );
}
