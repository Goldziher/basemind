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
use basemind::git_history::fts::FtsScope;

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
    assert_eq!(index.commit_count(), 1, "index reflects the rewound history");
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
    let indexed: Vec<String> = index.recent_commits(0, 10, false).into_iter().map(|c| c.sha).collect();
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
        eprintln!("no git-history index at {repo_root}; run `basemind scan` there first — skipping");
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
    eprintln!("commits_touching warm: {n} queries in {elapsed:?} = {per_us:.1} µs/query ({hits} total hits)");
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

/// Commit `path=content` authored by a specific name/email, with a subject line and an optional
/// body (git joins the two `-m` args as `subject\n\nbody`).
fn commit_authored(root: &Path, path: &str, content: &str, name: &str, email: &str, subject: &str, body: &str) {
    fs::write(root.join(path), content).unwrap();
    run(root, &["add", path]);
    let mut args = vec!["commit", "-q", "-m", subject];
    if !body.is_empty() {
        args.push("-m");
        args.push(body);
    }
    let status = Command::new("git")
        .args(&args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", name)
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_COMMITTER_NAME", name)
        .env("GIT_COMMITTER_EMAIL", email)
        .env("GIT_AUTHOR_DATE", "2025-01-01T00:00:00")
        .env("GIT_COMMITTER_DATE", "2025-01-01T00:00:00")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git commit failed");
}

fn search_shas(index: &GitHistoryIndex, query: &str, scope: FtsScope) -> Vec<String> {
    index
        .search_commits(query, scope, 0, 100)
        .into_iter()
        .map(|c| c.sha)
        .collect()
}

#[test]
fn full_text_search_over_author_message_and_body() {
    let dir = init_repo();
    let root = dir.path();
    commit_authored(
        root,
        "adder.rs",
        "fn add() {}\n",
        "Ada Lovelace",
        "ada@calc.example",
        "feat: add adder",
        "Implements the addition routine.\nCloses #42.",
    );
    commit_authored(
        root,
        "parser.rs",
        "fn p() {}\n",
        "Bjorn Iron",
        "bjorn@forge.example",
        "fix: null deref",
        "Guard against a nil pointer in the parser.",
    );
    commit_authored(
        root,
        "README.md",
        "# hi\n",
        "Ada Lovelace",
        "ada@calc.example",
        "docs: readme",
        "", // summary-only: no body row written
    );

    let (index, _) = sync(root);

    let all_by_ada = search_shas(&index, "ada", FtsScope::Author);
    assert_eq!(all_by_ada.len(), 2, "two commits authored by Ada, got {all_by_ada:?}");

    // Author email token matches the author field.
    assert_eq!(
        search_shas(&index, "calc", FtsScope::Author).len(),
        2,
        "email host token 'calc' matches both of Ada's commits"
    );
    // 'ada' is an author token, not a message token → message scope finds nothing.
    assert!(search_shas(&index, "ada", FtsScope::Message).is_empty());

    // Message/summary search.
    assert_eq!(search_shas(&index, "adder", FtsScope::All).len(), 1);
    // Body-only word (not in the summary) proves the body is indexed.
    let addition = index.search_commits("addition", FtsScope::Message, 0, 10);
    assert_eq!(addition.len(), 1, "body-only term 'addition' finds the feat commit");
    assert!(
        addition[0].body.contains("Implements the addition routine."),
        "search result carries the full body, got {:?}",
        addition[0].body
    );

    // AND semantics: both terms in one commit's message → match.
    assert_eq!(search_shas(&index, "null deref", FtsScope::Message).len(), 1);
    // Terms that live in DIFFERENT commits → no single commit satisfies the AND.
    assert!(
        search_shas(&index, "adder parser", FtsScope::All).is_empty(),
        "'adder' (c1) AND 'parser' (c2) share no commit"
    );
    // Empty / punctuation-only query → nothing.
    assert!(search_shas(&index, "   ", FtsScope::All).is_empty());
    drop(index);

    // Incremental append keeps the term index current.
    commit_authored(
        root,
        "vector.rs",
        "fn v() {}\n",
        "Carl Gauss",
        "carl@sum.example",
        "perf: vectorize adder",
        "Faster addition via SIMD.",
    );
    let (index, outcome) = sync(root);
    assert!(matches!(outcome, RebuildOutcome::Incremental { added: 1 }));
    // 'adder' now appears in two commits (c1 feat + c4 perf), newest-first.
    assert_eq!(search_shas(&index, "adder", FtsScope::All).len(), 2);
    // New author is searchable after the incremental append.
    assert_eq!(search_shas(&index, "gauss", FtsScope::Author).len(), 1);
    // 'addition' now in c1 body + c4 body.
    assert_eq!(search_shas(&index, "addition", FtsScope::Message).len(), 2);
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

/// Commit authored by `name`/`email` at an explicit ISO date, so the newest-first walk order is
/// unambiguous even across many commits (the shared `commit_authored` pins a single timestamp,
/// which leaves same-second ordering to the traversal — fine for a handful of commits, not for the
/// deep history this test builds).
fn commit_authored_at(root: &Path, path: &str, content: &str, name: &str, email: &str, subject: &str, date: &str) {
    fs::write(root.join(path), content).unwrap();
    run(root, &["add", path]);
    let status = Command::new("git")
        .args(["commit", "-q", "-m", subject])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", name)
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_COMMITTER_NAME", name)
        .env("GIT_COMMITTER_EMAIL", email)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git commit failed");
}

/// Regression for the reported precision bug: an author whose latest commit is buried deeper than
/// the `recent_changes` window (limit ≤ 100). `search_git_history(field=author)` must still find it
/// — matching `git log -i --author` at full branch depth — while the newest-100 window does not
/// contain it, which is exactly why routing an author question to `recent_changes` returned "no
/// commit by that author". Branch/HEAD-scoped throughout (no `--all`).
#[test]
fn author_search_finds_commit_beyond_recent_window_matches_git() {
    let dir = init_repo();
    let root = dir.path();

    // Oldest commit (rank 121): the author we later look for.
    commit_authored_at(
        root,
        "a.rs",
        "fn a() {}\n",
        "Dor Green",
        "dor@example.com",
        "early: seed the module",
        "2025-01-01T00:00:00",
    );
    // Bury it under 120 strictly-later commits by a different author.
    for i in 0..120 {
        let date = format!("2025-01-01T{:02}:{:02}:00", 1 + (i / 60), i % 60);
        commit_authored_at(
            root,
            "b.rs",
            &format!("v{i}\n"),
            "Other Dev",
            "other@example.com",
            &format!("chore: change {i}"),
            &date,
        );
    }

    let (index, _) = sync(root);
    assert_eq!(index.commit_count(), 121);

    // Oracle: git's own HEAD-scoped, case-insensitive author search, newest first.
    let git_newest = capture(root, &["log", "-i", "--author=Dor Green", "--format=%H", "-1"])
        .trim()
        .to_string();
    assert!(!git_newest.is_empty(), "git finds Dor Green's commit");

    // search_git_history(author) returns exactly Dor's commit — full-depth, matching git.
    assert_eq!(
        search_shas(&index, "Dor Green", FtsScope::Author),
        vec![git_newest.clone()],
        "author search matches `git log -i --author` at full depth"
    );

    // And it lives beyond the newest-100 window that `recent_changes` scans — the bug's root cause.
    let recent: Vec<String> = index.recent_commits(0, 100, false).into_iter().map(|c| c.sha).collect();
    assert_eq!(recent.len(), 100, "the recent window is capped at 100");
    assert!(
        !recent.contains(&git_newest),
        "Dor's commit is outside the newest-100 window, so a recent-window scan misses it"
    );
}
