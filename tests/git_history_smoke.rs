//! Integration tests for the git-history index: revalidation (fresh / incremental / rewrite),
//! and query parity against the `git` CLI on real tempdir repositories.
//!
//! The index is built directly via `GitHistoryIndex::open` + `builder::sync` (no full file scan
//! needed) so these stay fast. The `git` CLI sets up fixtures and serves as the correctness oracle,
//! exactly mirroring `tests/git_smoke.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use basemind::git::Repo;
use basemind::git_history::GitHistoryIndex;
use basemind::git_history::builder::{self, RebuildOutcome};

fn run(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        // Deterministic, monotonically increasing commit times so the newest-first walk order is
        // stable (rev-walk sorts by commit time).
        .env("GIT_AUTHOR_DATE", "2025-01-01T00:00:00")
        .env("GIT_COMMITTER_DATE", "2025-01-01T00:00:00")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

fn capture(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git in PATH");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout).expect("utf8")
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    run(dir.path(), &["init", "-q", "-b", "main"]);
    run(dir.path(), &["config", "commit.gpgsign", "false"]);
    dir
}

/// Commit `path=content` with message `msg`.
fn commit_file(root: &Path, path: &str, content: &str, msg: &str) {
    fs::write(root.join(path), content).unwrap();
    run(root, &["add", path]);
    run(root, &["commit", "-qm", msg]);
}

fn basemind_dir(root: &Path) -> PathBuf {
    let dir = root.join(".basemind");
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Open the index and bring it in sync with HEAD.
fn sync(root: &Path) -> (GitHistoryIndex, RebuildOutcome) {
    let repo = Repo::discover(root).expect("repo discover");
    let bdir = basemind_dir(root);
    let index = GitHistoryIndex::open(&bdir).expect("open git-history index");
    let outcome = builder::sync(&index, &repo, &bdir).expect("sync");
    (index, outcome)
}

/// Newest-first 40-hex shas of commits touching `path`, per the `git` CLI (the oracle). Uses
/// `--full-history` to match the index's union-across-parents, exact-path semantics.
fn git_commits_touching(root: &Path, path: &str) -> Vec<String> {
    capture(root, &["log", "--full-history", "--format=%H", "--", path])
        .lines()
        .map(|s| s.to_string())
        .collect()
}

fn index_commits_touching(index: &GitHistoryIndex, path: &str) -> Vec<String> {
    index
        .commits_touching(&path.as_bytes().into(), 0, 1000)
        .into_iter()
        .map(|c| c.sha)
        .collect()
}

#[test]
fn full_rebuild_then_fresh_then_incremental() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "a.rs", "fn a1() {}\n", "c1");
    commit_file(root, "b.rs", "fn b1() {}\n", "c2");
    commit_file(root, "a.rs", "fn a2() {}\n", "c3");

    // First sync: full rebuild over all three commits.
    let (index, outcome) = sync(root);
    assert_eq!(
        outcome,
        RebuildOutcome::FullRebuild {
            reason: "initial",
            commits: 3
        }
    );
    assert_eq!(index.commit_count(), 3);
    assert_eq!(
        index_commits_touching(&index, "a.rs"),
        git_commits_touching(root, "a.rs"),
        "indexed history of a.rs matches git"
    );
    // Drop the handle before re-opening — Fjall takes an exclusive per-directory process lock, so
    // only one open handle may exist at a time (the single-writer model the real code relies on).
    drop(index);

    // Second sync, HEAD unchanged: fresh no-op.
    let (index, outcome) = sync(root);
    assert_eq!(outcome, RebuildOutcome::Fresh);
    drop(index);

    // Add a commit, then sync: incremental append of exactly one commit.
    commit_file(root, "a.rs", "fn a3() {}\n", "c4");
    let (index, outcome) = sync(root);
    assert_eq!(outcome, RebuildOutcome::Incremental { added: 1 });
    assert_eq!(index.commit_count(), 4);
    // The newest commit touching a.rs is the one just added; full history still matches git.
    assert_eq!(
        index_commits_touching(&index, "a.rs"),
        git_commits_touching(root, "a.rs"),
        "incremental append preserves old ords and adds the new commit"
    );
    // A file untouched by the new commit still resolves to its original history.
    assert_eq!(
        index_commits_touching(&index, "b.rs"),
        git_commits_touching(root, "b.rs"),
    );
}

#[test]
fn history_rewrite_triggers_full_rebuild_no_stale_commits() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "a.rs", "fn a1() {}\n", "c1");
    commit_file(root, "a.rs", "fn a2() {}\n", "c2");

    let (index, _) = sync(root);
    let before = index_commits_touching(&index, "a.rs");
    assert_eq!(before.len(), 2);
    drop(index);

    // Rewrite history: amend the tip (changes its sha) — the filter-repo class of event.
    run(root, &["commit", "--amend", "-qm", "c2-amended"]);
    let rewritten = git_commits_touching(root, "a.rs");
    assert_ne!(before, rewritten, "amend changed the tip sha");

    let (index, outcome) = sync(root);
    assert!(
        matches!(
            outcome,
            RebuildOutcome::FullRebuild {
                reason: "history-rewrite",
                ..
            }
        ),
        "rewrite forces a full rebuild, got {outcome:?}"
    );
    // No stale commits: the index now reflects the rewritten history exactly.
    assert_eq!(index_commits_touching(&index, "a.rs"), rewritten);
}

#[test]
fn reset_back_diverges_and_triggers_full_rebuild() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "a.rs", "fn a1() {}\n", "c1");
    commit_file(root, "a.rs", "fn a2() {}\n", "c2");
    commit_file(root, "a.rs", "fn a3() {}\n", "c3");

    let (index, _) = sync(root);
    assert_eq!(index.commit_count(), 3);
    drop(index);

    // Move HEAD backwards: the last indexed head is no longer an ancestor of HEAD.
    run(root, &["reset", "--hard", "-q", "HEAD~2"]);
    let (index, outcome) = sync(root);
    assert!(
        matches!(outcome, RebuildOutcome::FullRebuild { .. }),
        "reset-back forces a full rebuild, got {outcome:?}"
    );
    assert_eq!(
        index.commit_count(),
        1,
        "index reflects the rewound history"
    );
    assert_eq!(
        index_commits_touching(&index, "a.rs"),
        git_commits_touching(root, "a.rs"),
    );
}

#[test]
fn merge_commit_union_across_parents_matches_git() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "base.rs", "fn base() {}\n", "c1");
    // Branch off and change shared.rs on a feature branch.
    run(root, &["checkout", "-qb", "feature"]);
    commit_file(root, "shared.rs", "fn feature() {}\n", "feat");
    run(root, &["checkout", "-q", "main"]);
    // Diverge on master too so the merge is a real two-parent merge.
    commit_file(root, "base.rs", "fn base2() {}\n", "main2");
    run(root, &["merge", "-q", "--no-ff", "feature", "-m", "merge"]);

    let (index, _) = sync(root);
    // shared.rs came in on the feature leg; the merge is union-across-parents, so the index must
    // agree with `git log --full-history` for the path.
    assert_eq!(
        index_commits_touching(&index, "shared.rs"),
        git_commits_touching(root, "shared.rs"),
        "merge handling matches git full-history"
    );
    assert_eq!(
        index_commits_touching(&index, "base.rs"),
        git_commits_touching(root, "base.rs"),
    );
}

#[test]
fn recent_commits_newest_first_matches_git() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "a.rs", "1\n", "c1");
    commit_file(root, "b.rs", "2\n", "c2");
    commit_file(root, "c.rs", "3\n", "c3");

    let (index, _) = sync(root);
    let indexed: Vec<String> = index
        .recent_commits(0, 10, false)
        .into_iter()
        .map(|c| c.sha)
        .collect();
    let git: Vec<String> = capture(root, &["log", "--format=%H"])
        .lines()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(indexed, git, "recent_commits is newest-first like git log");
}

/// Warm in-process query latency against a prebuilt index (no per-call process startup, unlike the
/// CLI). Point `BASEMIND_BENCH_REPO` at a scanned repo to run it; skips cleanly when that env var is
/// unset or the index isn't present. Run with:
/// `BASEMIND_BENCH_REPO=/path/to/repo cargo test --release --test git_history_smoke -- --ignored \
///   --nocapture bench_warm_query_latency`
#[test]
#[ignore]
fn bench_warm_query_latency() {
    let Ok(repo_root) = std::env::var("BASEMIND_BENCH_REPO") else {
        eprintln!("set BASEMIND_BENCH_REPO to a scanned repo to run this bench — skipping");
        return;
    };
    let bdir = Path::new(&repo_root).join(".basemind");
    if !bdir.join("git-history.fjall").exists() {
        eprintln!(
            "no git-history index at {repo_root}; run `basemind scan` there first — skipping"
        );
        return;
    }
    let index = GitHistoryIndex::open(&bdir).expect("open index");
    // Walk the newest commits to harvest real paths to query (mix of rare + hot).
    let sample: Vec<String> = index
        .recent_commits(0, 200, true)
        .into_iter()
        .flat_map(|c| c.files.into_iter().map(|(p, _)| p.to_string()))
        .take(500)
        .collect();
    assert!(!sample.is_empty(), "index has commits to sample paths from");

    // Warm the block cache.
    for p in sample.iter().take(50) {
        let _ = index.commits_touching(&p.as_bytes().into(), 0, 20);
    }
    let n = 2000usize;
    let start = std::time::Instant::now();
    let mut hits = 0usize;
    for i in 0..n {
        let p = &sample[i % sample.len()];
        hits += index.commits_touching(&p.as_bytes().into(), 0, 20).len();
    }
    let elapsed = start.elapsed();
    let per_us = elapsed.as_micros() as f64 / n as f64;
    eprintln!(
        "commits_touching warm: {n} queries in {elapsed:?} = {per_us:.1} µs/query ({hits} total hits)"
    );
    assert!(
        per_us < 1000.0,
        "warm commits_touching must be well under 1ms, got {per_us:.1} µs"
    );
}

/// Manual peak-RSS / wall-time harness for a full rebuild against a real repo. Clears and rebuilds
/// the git-history index for the repo named by `BASEMIND_BENCH_REPO`, so wrap it in a memory profiler
/// to see the builder's peak resident set:
/// `BASEMIND_BENCH_REPO=/path/to/repo /usr/bin/time -l cargo test --release \
///   --test git_history_smoke -- --ignored --nocapture bench_rebuild_peak_rss`
/// The chunked fold (see `builder::RECORD_CHUNK`) caps resident commit records at one chunk. Skips
/// cleanly when the env var is unset or the repo isn't present.
#[test]
#[ignore]
fn bench_rebuild_peak_rss() {
    let Ok(repo_root) = std::env::var("BASEMIND_BENCH_REPO") else {
        eprintln!("set BASEMIND_BENCH_REPO to a git repo to run this bench — skipping");
        return;
    };
    let root = Path::new(&repo_root);
    if !root.join(".git").exists() {
        eprintln!("no git repo at {repo_root}; skipping");
        return;
    }
    let repo = Repo::discover(root).expect("discover");
    let bdir = root.join(".basemind");
    std::fs::create_dir_all(&bdir).unwrap();
    let index = GitHistoryIndex::open(&bdir).expect("open");
    // Force a from-scratch rebuild so the run measures the full build, not an incremental no-op.
    index.clear(&bdir).expect("clear");
    let start = std::time::Instant::now();
    let outcome = builder::sync(&index, &repo, &bdir).expect("sync");
    let elapsed = start.elapsed();
    eprintln!(
        "rebuild outcome={outcome:?} in {elapsed:?} (commit_count={})",
        index.commit_count()
    );
    assert!(index.commit_count() > 0, "rebuilt index has commits");
}

#[test]
fn empty_index_before_sync_falls_back() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "a.rs", "fn a() {}\n", "c1");
    let bdir = basemind_dir(root);
    let index = GitHistoryIndex::open(&bdir).expect("open");
    // Never synced: empty, and not fresh for any head (so tools would live-walk).
    assert!(index.is_empty());
    assert_eq!(index.last_indexed_head_hex(), None);
}
