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
    assert_eq!(shallow_log, std::slice::from_ref(&head));
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
            reason: "deepened",
            commits: 3
        }
    );
    assert_eq!(index.commit_count(), 3);
    assert_eq!(
        builder::sync(&index, &repo, &basemind_dir).unwrap(),
        RebuildOutcome::Fresh
    );
}

/// Commit every change in `root` with author and committer time on `day` of January 2025, so a
/// walk by commit time has one order.
fn commit_on(root: &Path, day: u32, message: &str) {
    let date = format!("2025-01-{day:02}T00:00:00");
    let status = Command::new("git")
        .args(["commit", "-qam", message])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git commit {message:?} failed");
}

/// A `--depth 3` clone whose boundary commit `c2` is the third of five upstream commits:
/// `c0` adds `a.rs` and `b.rs`, `c1` changes `b.rs`, `c2` adds `c.rs`, `c3` changes `b.rs`, and
/// `c4` changes `c.rs`. Returns the clone's path and the upstream shas, `c0` first.
fn depth_3_clone() -> (TempDir, PathBuf, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let upstream = dir.path().join("upstream");
    fs::create_dir_all(&upstream).unwrap();
    run(&upstream, &["init", "-q"]);
    run(&upstream, &["config", "commit.gpgsign", "false"]);
    let steps: [(&str, &str); 5] = [
        ("a.rs", "a\n"),
        ("b.rs", "b 1\n"),
        ("c.rs", "c\n"),
        ("b.rs", "b 2\n"),
        ("c.rs", "c 1\n"),
    ];
    fs::write(upstream.join("b.rs"), "b\n").unwrap();
    let mut shas = Vec::new();
    for (day, (file, content)) in (1..).zip(steps) {
        fs::write(upstream.join(file), content).unwrap();
        run(&upstream, &["add", "-A"]);
        commit_on(&upstream, day, &format!("c{}", day - 1));
        shas.push(Repo::discover(&upstream).unwrap().resolve_rev("HEAD").unwrap());
    }
    let url = format!("file://{}", upstream.display());
    run(dir.path(), &["clone", "-q", "--depth", "3", &url, "clone"]);
    let clone = dir.path().join("clone");
    (dir, clone, shas)
}

#[test]
fn the_history_index_and_the_live_walk_agree_on_a_shallow_clones_boundary_commit() {
    let (_dir, clone, c) = depth_3_clone();
    let repo = Repo::discover(&clone).unwrap();
    assert!(repo.is_shallow());
    let basemind_dir = clone.join(".basemind");
    fs::create_dir_all(&basemind_dir).unwrap();
    let index = GitHistoryIndex::open(&basemind_dir).unwrap();
    assert_eq!(
        builder::sync(&index, &repo, &basemind_dir).unwrap(),
        RebuildOutcome::FullRebuild {
            reason: "initial",
            commits: 3
        }
    );

    let expected = [
        ("a.rs", vec![c[2].clone()]),
        ("b.rs", vec![c[3].clone(), c[2].clone()]),
        ("c.rs", vec![c[4].clone(), c[2].clone()]),
    ];
    for (path, touching) in expected {
        let live: Vec<String> = repo
            .log_for_path(path, 10)
            .unwrap()
            .into_iter()
            .map(|commit| commit.sha)
            .collect();
        let indexed: Vec<String> = index
            .commits_touching(&RelPath::from(path), 0, 10)
            .into_iter()
            .map(|commit| commit.sha)
            .collect();
        assert_eq!(live, touching, "live walk of {path}");
        assert_eq!(indexed, touching, "git-history index for {path}");
    }
    let boundary: Vec<(String, ChangeKind)> = repo
        .commit_files_uncached(&c[2])
        .unwrap()
        .into_iter()
        .map(|(path, kind)| (path.to_str_lossy().into_owned(), kind))
        .collect();
    assert_eq!(
        boundary,
        [
            ("a.rs".to_string(), ChangeKind::Added),
            ("b.rs".to_string(), ChangeKind::Added),
            ("c.rs".to_string(), ChangeKind::Added),
        ]
    );
}
