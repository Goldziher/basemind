//! Integration tests for the git-history index: revalidation (fresh / incremental / rewrite),
//! and query parity against the `git` CLI on real tempdir repositories.
//!
//! The index is built directly via `GitHistoryIndex::open` + `builder::sync` (no full file scan
//! needed) so these stay fast. The `git` CLI sets up fixtures and serves as the correctness oracle,
//! exactly mirroring `tests/git_smoke.rs`.

use std::fs;
use std::path::{Path, PathBuf};

use basemind::git::Repo;
use basemind::git_history::GitHistoryIndex;
use basemind::git_history::builder::{self, RebuildOutcome};
use basemind::git_history::fts::FtsScope;

fn run(repo: &Path, args: &[&str]) {
    let status = basemind::testing::git_command()
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_DATE", "2025-01-01T00:00:00")
        .env("GIT_COMMITTER_DATE", "2025-01-01T00:00:00")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

fn capture(repo: &Path, args: &[&str]) -> String {
    let out = basemind::testing::git_command()
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

/// Newest-first 40-hex shas of commits touching `path`, per the `git` CLI: `git log -- <path>`. It
/// agrees with basemind's rule where no side of a merge that git prunes changed `path`; see
/// [`full_history_touching`] for the rule itself.
fn git_commits_touching(root: &Path, path: &str) -> Vec<String> {
    capture(root, &["log", "--format=%H", "--", path])
        .lines()
        .map(|s| s.to_string())
        .collect()
}

fn index_commits_touching(index: &GitHistoryIndex, path: &str) -> Vec<String> {
    index
        .commits_touching(&path.as_bytes().into(), 0, 1000)
        .expect("index read")
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
    drop(index);

    let (index, outcome) = sync(root);
    assert_eq!(outcome, RebuildOutcome::Fresh);
    drop(index);

    commit_file(root, "a.rs", "fn a3() {}\n", "c4");
    let (index, outcome) = sync(root);
    assert_eq!(outcome, RebuildOutcome::Incremental { added: 1 });
    assert_eq!(index.commit_count(), 4);
    assert_eq!(
        index_commits_touching(&index, "a.rs"),
        git_commits_touching(root, "a.rs"),
        "incremental append preserves old ords and adds the new commit"
    );
    assert_eq!(
        index_commits_touching(&index, "b.rs"),
        git_commits_touching(root, "b.rs"),
    );
}

/// The scan and the standalone serve drop the index after a sync instead of leaking it (a leaked
/// fjall database is a definite leak under valgrind and keeps its lock until exit). That is only
/// sound if closing a just-synced database returns: fjall before 3.1.13 could block forever in
/// `DatabaseInner::drop` on a full worker channel (fjall-rs/fjall#260).
#[test]
fn a_synced_index_closes_promptly_and_releases_its_lock() {
    let dir = init_repo();
    let root = dir.path();
    for i in 0..20 {
        commit_file(root, "a.rs", &format!("fn a{i}() {{}}\n"), &format!("c{i}"));
    }
    let (index, outcome) = sync(root);
    assert!(
        matches!(outcome, RebuildOutcome::FullRebuild { commits: 20, .. }),
        "{outcome:?}"
    );

    let (done, closed) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(index);
        let _ = done.send(());
    });
    closed
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("dropping a synced git-history index returns");
    let reopened = GitHistoryIndex::open_once(&basemind_dir(root)).expect("the dropped index released its lock");
    assert_eq!(reopened.commit_count(), 20);
}

/// Newest-first shas of commits touching `path` as `head` sees them in the index.
fn touching_at(index: &GitHistoryIndex, head: &str, path: &str) -> Vec<String> {
    index
        .at(head)
        .unwrap_or_else(|| panic!("{head} is an indexed tip"))
        .commits_touching(&path.as_bytes().into(), 0, 1000)
        .expect("index read")
        .into_iter()
        .map(|c| c.sha)
        .collect()
}

fn head_of(root: &Path) -> String {
    capture(root, &["rev-parse", "HEAD"]).trim().to_string()
}

#[test]
fn an_amended_head_appends_the_rewritten_commit_and_serves_only_its_history() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "a.rs", "fn a1() {}\n", "c1");
    commit_file(root, "a.rs", "fn a2() {}\n", "c2");

    let (index, _) = sync(root);
    let before = index_commits_touching(&index, "a.rs");
    assert_eq!(before.len(), 2);
    let old_head = head_of(root);
    drop(index);

    run(root, &["commit", "--amend", "-qm", "c2-amended"]);
    let rewritten = git_commits_touching(root, "a.rs");
    assert_ne!(before, rewritten, "amend changed the tip sha");

    let (index, outcome) = sync(root);
    assert_eq!(
        outcome,
        RebuildOutcome::Incremental { added: 1 },
        "no wipe, one commit appended"
    );
    assert_eq!(index_commits_touching(&index, "a.rs"), rewritten);
    assert_eq!(
        index.commit_count(),
        3,
        "the replaced commit stays stored for the head that still has it"
    );
    assert_eq!(
        touching_at(&index, &old_head, "a.rs"),
        before,
        "the old head still answers"
    );
}

#[test]
fn reset_back_appends_nothing_and_serves_the_rewound_history() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "a.rs", "fn a1() {}\n", "c1");
    commit_file(root, "a.rs", "fn a2() {}\n", "c2");
    commit_file(root, "a.rs", "fn a3() {}\n", "c3");

    let (index, _) = sync(root);
    assert_eq!(index.commit_count(), 3);
    let tip = head_of(root);
    drop(index);

    run(root, &["reset", "--hard", "-q", "HEAD~2"]);
    let (index, outcome) = sync(root);
    assert_eq!(
        outcome,
        RebuildOutcome::Incremental { added: 0 },
        "every commit is already stored"
    );
    assert_eq!(index.commit_count(), 3, "nothing is wiped");
    assert_eq!(
        index_commits_touching(&index, "a.rs"),
        git_commits_touching(root, "a.rs"),
        "the rewound head sees only its own commit"
    );
    assert_eq!(
        touching_at(&index, &tip, "a.rs").len(),
        3,
        "the former head still sees all three"
    );
}

/// Newest-first shas of commits touching `path` per the live walk, the other half of `git_log
/// {path}`.
fn live_commits_touching(root: &Path, path: &str) -> Vec<String> {
    let repo = Repo::discover(root).expect("repo discover");
    let head = capture(root, &["rev-parse", "HEAD"]);
    repo.touching_span(head.trim(), &path.as_bytes().into(), 0, 1000, usize::MAX)
        .expect("live walk")
        .commits
        .into_iter()
        .map(|(_, c)| c.sha)
        .collect()
}

/// A merge that only brings a branch in changes no path of its own: the index and the live walk
/// leave it out, as `git log -- <path>` does here. One that changes a path against both parents (an
/// evil merge) touches it.
#[test]
fn a_merge_touches_only_the_paths_it_changes_against_every_parent() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "base.rs", "fn base() {}\n", "c1");
    run(root, &["checkout", "-qb", "feature"]);
    commit_file(root, "shared.rs", "fn feature() {}\n", "feat");
    run(root, &["checkout", "-q", "main"]);
    commit_file(root, "base.rs", "fn base2() {}\n", "main2");
    run(root, &["merge", "-q", "--no-ff", "feature", "-m", "merge"]);
    fs::write(root.join("base.rs"), "fn evil() {}\n").unwrap();
    run(root, &["commit", "-q", "--amend", "-am", "merge"]);
    let merge = capture(root, &["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(
        capture(root, &["rev-list", "--parents", "-n1", "HEAD"])
            .split_whitespace()
            .count(),
        3
    );

    let (index, _) = sync(root);
    for path in ["shared.rs", "base.rs"] {
        let git = git_commits_touching(root, path);
        assert_eq!(index_commits_touching(&index, path), git, "index: {path}");
        assert_eq!(live_commits_touching(root, path), git, "live walk: {path}");
    }
    assert!(!git_commits_touching(root, "shared.rs").contains(&merge));
    assert!(git_commits_touching(root, "base.rs").contains(&merge));

    let repo = Repo::discover(root).expect("repo discover");
    let files = repo.commit_record(&merge).expect("merge record").files;
    assert_eq!(
        files,
        [("base.rs".into(), basemind::git::ChangeKind::Modified)],
        "a merge lists what it changed against every parent"
    );
}

/// basemind's rule per the `git` CLI: every commit `git log --full-history -- <path>` lists, less
/// each merge that equals one of its parents at `path`. Unlike plain `git log -- <path>`, no side
/// of a merge is pruned. Sorted, since the fixtures' commits share one timestamp.
fn full_history_touching(root: &Path, path: &str) -> Vec<String> {
    let mut shas: Vec<String> = capture(root, &["log", "--full-history", "--format=%H %P", "--", path])
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let sha = fields.next()?.to_string();
            let parents: Vec<&str> = fields.collect();
            let from_a_parent = parents.len() > 1
                && parents.iter().any(|parent| {
                    basemind::testing::git_command()
                        .args(["diff", "--quiet", parent, &sha, "--", path])
                        .current_dir(root)
                        .status()
                        .expect("git in PATH")
                        .success()
                });
            (!from_a_parent).then_some(sha)
        })
        .collect();
    shas.sort();
    shas
}

/// A conflicted merge resolved to one side's version equals that parent at the path, so it is not
/// listed; both sides' own commits are. Plain `git log -- <path>` follows only the side the merge
/// kept and hides the other side's commit; basemind lists it, since that commit did change the path.
#[test]
fn a_merge_resolved_to_one_side_lists_both_sides_commits_and_not_the_merge() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "x.rs", "fn x() {}\n", "c1");
    run(root, &["checkout", "-qb", "feature"]);
    commit_file(root, "x.rs", "fn feature() {}\n", "f1");
    let f1 = capture(root, &["rev-parse", "HEAD"]).trim().to_string();
    run(root, &["checkout", "-q", "main"]);
    commit_file(root, "x.rs", "fn main_side() {}\n", "m1");
    let m1 = capture(root, &["rev-parse", "HEAD"]).trim().to_string();
    let conflicted = basemind::testing::git_command()
        .args(["merge", "-q", "--no-ff", "feature", "-m", "merge"])
        .current_dir(root)
        .output()
        .expect("git in PATH");
    assert!(
        !conflicted.status.success(),
        "both sides changed x.rs, so the merge conflicts"
    );
    run(root, &["checkout", "--theirs", "x.rs"]);
    run(root, &["add", "x.rs"]);
    run(root, &["commit", "-qm", "merge"]);
    let merge = capture(root, &["rev-parse", "HEAD"]).trim().to_string();

    let rule = full_history_touching(root, "x.rs");
    assert!(
        rule.contains(&m1) && rule.contains(&f1) && !rule.contains(&merge),
        "{rule:?}"
    );
    assert!(
        !git_commits_touching(root, "x.rs").contains(&m1),
        "plain `git log` prunes the side the merge did not keep, which basemind does not"
    );
    let (index, _) = sync(root);
    let sorted = |mut shas: Vec<String>| {
        shas.sort();
        shas
    };
    assert_eq!(sorted(index_commits_touching(&index, "x.rs")), rule, "index");
    assert_eq!(sorted(live_commits_touching(root, "x.rs")), rule, "live walk");
}

/// A commit's file list names files. The tree diff also reports each directory on the way to a
/// changed file, and those are not changes of their own.
#[test]
fn a_commit_lists_the_files_it_changed_and_no_directory() {
    let dir = init_repo();
    let root = dir.path();
    fs::create_dir_all(root.join("src/deep")).unwrap();
    commit_file(root, "src/deep/a.rs", "fn a1() {}\n", "add");
    commit_file(root, "src/deep/a.rs", "fn a2() {}\n", "edit");
    let head = capture(root, &["rev-parse", "HEAD"]);

    let expected = [("src/deep/a.rs".into(), basemind::git::ChangeKind::Modified)];
    let repo = Repo::discover(root).expect("repo discover");
    assert_eq!(repo.commit_record(head.trim()).expect("record").files, expected);
    let (index, _) = sync(root);
    assert_eq!(index.recent_commits(0, 1, true)[0].files, expected, "indexed file list");
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
    let sample: Vec<String> = index
        .recent_commits(0, 200, true)
        .into_iter()
        .flat_map(|c| c.files.into_iter().map(|(p, _)| p.to_string()))
        .take(500)
        .collect();
    assert!(!sample.is_empty(), "index has commits to sample paths from");

    for p in sample.iter().take(50) {
        let _ = index.commits_touching(&p.as_bytes().into(), 0, 20).expect("index read");
    }
    let n = 2000usize;
    let start = std::time::Instant::now();
    let mut hits = 0usize;
    for i in 0..n {
        let p = &sample[i % sample.len()];
        hits += index
            .commits_touching(&p.as_bytes().into(), 0, 20)
            .expect("index read")
            .len();
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
    index.clear().expect("clear");
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
    let status = basemind::testing::git_command()
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
        "",
    );

    let (index, _) = sync(root);

    let all_by_ada = search_shas(&index, "ada", FtsScope::Author);
    assert_eq!(all_by_ada.len(), 2, "two commits authored by Ada, got {all_by_ada:?}");

    assert_eq!(
        search_shas(&index, "calc", FtsScope::Author).len(),
        2,
        "email host token 'calc' matches both of Ada's commits"
    );
    assert!(search_shas(&index, "ada", FtsScope::Message).is_empty());

    assert_eq!(search_shas(&index, "adder", FtsScope::All).len(), 1);
    let addition = index.search_commits("addition", FtsScope::Message, 0, 10);
    assert_eq!(addition.len(), 1, "body-only term 'addition' finds the feat commit");
    assert!(
        addition[0].body.contains("Implements the addition routine."),
        "search result carries the full body, got {:?}",
        addition[0].body
    );

    assert_eq!(search_shas(&index, "null deref", FtsScope::Message).len(), 1);
    assert!(
        search_shas(&index, "adder parser", FtsScope::All).is_empty(),
        "'adder' (c1) AND 'parser' (c2) share no commit"
    );
    assert!(search_shas(&index, "   ", FtsScope::All).is_empty());
    drop(index);

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
    assert_eq!(search_shas(&index, "adder", FtsScope::All).len(), 2);
    assert_eq!(search_shas(&index, "gauss", FtsScope::Author).len(), 1);
    assert_eq!(search_shas(&index, "addition", FtsScope::Message).len(), 2);
}

#[test]
fn empty_index_before_sync_falls_back() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "a.rs", "fn a() {}\n", "c1");
    let bdir = basemind_dir(root);
    let index = GitHistoryIndex::open(&bdir).expect("open");
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
    let status = basemind::testing::git_command()
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

    commit_authored_at(
        root,
        "a.rs",
        "fn a() {}\n",
        "Dor Green",
        "dor@example.com",
        "early: seed the module",
        "2025-01-01T00:00:00",
    );
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

    let git_newest = capture(root, &["log", "-i", "--author=Dor Green", "--format=%H", "-1"])
        .trim()
        .to_string();
    assert!(!git_newest.is_empty(), "git finds Dor Green's commit");

    assert_eq!(
        search_shas(&index, "Dor Green", FtsScope::Author),
        vec![git_newest.clone()],
        "author search matches `git log -i --author` at full depth"
    );

    let recent: Vec<String> = index.recent_commits(0, 100, false).into_iter().map(|c| c.sha).collect();
    assert_eq!(recent.len(), 100, "the recent window is capped at 100");
    assert!(
        !recent.contains(&git_newest),
        "Dor's commit is outside the newest-100 window, so a recent-window scan misses it"
    );
}

/// Open the shared index at `bdir` and sync it from the checkout rooted at `root`. Mirrors
/// production, where every linked worktree resolves to the MAIN worktree's index directory.
fn sync_at(root: &Path, bdir: &Path) -> (GitHistoryIndex, RebuildOutcome) {
    let repo = Repo::discover(root).expect("repo discover");
    let index = GitHistoryIndex::open(bdir).expect("open git-history index");
    let outcome = builder::sync(&index, &repo, bdir).expect("sync");
    (index, outcome)
}

/// Add a linked worktree of `main` on a new branch `feature`, then amend its tip so the branch
/// diverges from `main`'s head (the amended commit does not descend from the original).
fn diverged_linked_worktree(main: &Path) -> (tempfile::TempDir, PathBuf) {
    let holder = tempfile::tempdir().expect("tempdir");
    let wt = holder.path().join("wt");
    run(
        main,
        &["worktree", "add", "-q", "-b", "feature", wt.to_str().expect("utf8")],
    );
    run(&wt, &["config", "commit.gpgsign", "false"]);
    run(&wt, &["commit", "--amend", "-qm", "diverged"]);
    (holder, wt)
}

/// Two worktrees on diverged branches share one store, and each is answered from it, with no
/// partial result: a worktree's own commits are visible to it and not to the other.
#[test]
fn two_diverged_worktrees_are_both_indexed_and_each_sees_only_its_own_history() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "a.rs", "fn a1() {}\n", "c1");
    commit_file(root, "a.rs", "fn a2() {}\n", "c2");
    let bdir = basemind_dir(root);

    let (index, outcome) = sync_at(root, &bdir);
    assert!(matches!(outcome, RebuildOutcome::FullRebuild { reason: "initial", .. }));
    let head_a = head_of(root);
    drop(index);

    let (_holder, wt) = diverged_linked_worktree(root);
    commit_file(&wt, "b.rs", "fn b1() {}\n", "on the branch");
    let head_b = head_of(&wt);
    assert_ne!(head_a, head_b, "worktree branch diverged from the indexed head");
    assert!(Repo::discover(&wt).expect("discover worktree").is_linked_worktree());

    let (index, outcome) = sync_at(&wt, &bdir);
    assert_eq!(
        outcome,
        RebuildOutcome::Incremental { added: 2 },
        "the amended commit and the new one"
    );
    assert_eq!(index.commit_count(), 4, "nothing was wiped");
    assert!(index.has_tip(&head_a) && index.has_tip(&head_b));
    drop(index);

    let (index, outcome) = sync_at(root, &bdir);
    assert_eq!(
        outcome,
        RebuildOutcome::Fresh,
        "the main checkout's head is still an indexed tip"
    );
    for (checkout, head) in [(root, &head_a), (wt.as_path(), &head_b)] {
        for path in ["a.rs", "b.rs"] {
            assert_eq!(
                touching_at(&index, head, path),
                git_commits_touching(checkout, path),
                "{path} at {head}"
            );
        }
        let recent: Vec<String> = index
            .at(head)
            .expect("tip")
            .recent_commits(0, 100, false)
            .into_iter()
            .map(|c| c.sha)
            .collect();
        assert_eq!(
            recent,
            capture(checkout, &["log", "--format=%H"])
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            "the recent log at {head}"
        );
    }
}

#[test]
fn a_head_that_is_not_an_indexed_tip_is_not_served() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "a.rs", "fn a1() {}\n", "c1");
    let (index, _) = sync(root);

    assert!(index.at(&head_of(root)).is_some());
    assert!(index.at(&"1".repeat(40)).is_none());
    assert!(index.at("not a sha").is_none());
}

#[test]
fn a_head_moved_in_one_worktree_appends_only_its_new_commits_and_keeps_the_other_head_served() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "a.rs", "fn a1() {}\n", "c1");
    let bdir = basemind_dir(root);
    let (index, _) = sync_at(root, &bdir);
    drop(index);

    let (_holder, wt) = diverged_linked_worktree(root);
    let (index, outcome) = sync_at(&wt, &bdir);
    assert_eq!(outcome, RebuildOutcome::Incremental { added: 1 });
    drop(index);
    let head_main = head_of(root);

    commit_file(&wt, "a.rs", "fn a2() {}\n", "wt move 1");
    commit_file(&wt, "a.rs", "fn a3() {}\n", "wt move 2");
    let (index, outcome) = sync_at(&wt, &bdir);
    assert_eq!(
        outcome,
        RebuildOutcome::Incremental { added: 2 },
        "only the commits no sync stored"
    );
    assert_eq!(index.commit_count(), 4);
    assert_eq!(
        touching_at(&index, &head_of(&wt), "a.rs"),
        git_commits_touching(&wt, "a.rs")
    );
    assert_eq!(
        touching_at(&index, &head_main, "a.rs"),
        git_commits_touching(root, "a.rs"),
        "the main checkout's head is unaffected by the move"
    );
}

#[test]
fn linked_worktree_descending_from_indexed_head_appends() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "a.rs", "fn a1() {}\n", "c1");
    let bdir = basemind_dir(root);

    let (index, _) = sync_at(root, &bdir);
    assert_eq!(index.commit_count(), 1);
    drop(index);

    let holder = tempfile::tempdir().expect("tempdir");
    let wt = holder.path().join("wt");
    run(
        root,
        &["worktree", "add", "-q", "-b", "feature", wt.to_str().expect("utf8")],
    );
    run(&wt, &["config", "commit.gpgsign", "false"]);
    commit_file(&wt, "a.rs", "fn a2() {}\n", "c2");
    commit_file(&wt, "a.rs", "fn a3() {}\n", "c3");
    assert!(Repo::discover(&wt).expect("discover worktree").is_linked_worktree());

    let (index, outcome) = sync_at(&wt, &bdir);
    assert_eq!(outcome, RebuildOutcome::Incremental { added: 2 });
    assert_eq!(index.commit_count(), 3);
    assert_eq!(
        index.last_indexed_head_hex(),
        Some(capture(&wt, &["rev-parse", "HEAD"]).trim().to_string())
    );
}

#[test]
fn linked_worktree_builds_an_empty_index() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "a.rs", "fn a1() {}\n", "c1");
    commit_file(root, "a.rs", "fn a2() {}\n", "c2");
    let bdir = basemind_dir(root);

    let holder = tempfile::tempdir().expect("tempdir");
    let wt = holder.path().join("wt");
    run(
        root,
        &["worktree", "add", "-q", "-b", "feature", wt.to_str().expect("utf8")],
    );
    assert!(Repo::discover(&wt).expect("discover worktree").is_linked_worktree());

    let (index, outcome) = sync_at(&wt, &bdir);
    assert!(
        matches!(
            outcome,
            RebuildOutcome::FullRebuild {
                reason: "initial",
                commits: 2
            }
        ),
        "empty index is built from a linked worktree, got {outcome:?}"
    );
    assert_eq!(index.commit_count(), 2);
    assert_eq!(
        index.last_indexed_head_hex(),
        Some(capture(&wt, &["rev-parse", "HEAD"]).trim().to_string())
    );
}

#[test]
fn main_checkout_still_appends_after_a_diverged_linked_worktree_is_indexed() {
    let dir = init_repo();
    let root = dir.path();
    commit_file(root, "a.rs", "fn a1() {}\n", "c1");
    let bdir = basemind_dir(root);

    let (index, _) = sync_at(root, &bdir);
    assert_eq!(index.commit_count(), 1);
    drop(index);

    let (_holder, wt) = diverged_linked_worktree(root);
    let (index, outcome) = sync_at(&wt, &bdir);
    assert_eq!(outcome, RebuildOutcome::Incremental { added: 1 }, "got {outcome:?}");
    drop(index);

    commit_file(root, "a.rs", "fn a2() {}\n", "c2");
    let (index, outcome) = sync_at(root, &bdir);
    assert_eq!(outcome, RebuildOutcome::Incremental { added: 1 });
    assert_eq!(index.commit_count(), 3);
    assert_eq!(index.last_indexed_head_hex(), Some(head_of(root)));
    assert_eq!(
        index_commits_touching(&index, "a.rs"),
        git_commits_touching(root, "a.rs")
    );
    assert_eq!(
        touching_at(&index, &head_of(&wt), "a.rs"),
        git_commits_touching(&wt, "a.rs"),
        "the worktree's head is still served"
    );
}

/// Newest-first shas of the live walk of HEAD, the order `git_log` answers in without the index.
fn live_walk(root: &Path) -> Vec<String> {
    Repo::discover(root)
        .expect("repo discover")
        .log_paths(1000, false)
        .expect("live walk")
        .into_iter()
        .map(|commit| commit.sha)
        .collect()
}

fn index_recent(index: &GitHistoryIndex, head: &str) -> Vec<String> {
    index
        .at(head)
        .unwrap_or_else(|| panic!("{head} is served from the index"))
        .recent_commits(0, 100, false)
        .into_iter()
        .map(|commit| commit.sha)
        .collect()
}

/// A head that extends the indexed tip linearly keeps the index as its fast path, and the index
/// lists its commits in the live walk's order.
#[test]
fn a_linear_head_is_served_from_the_index_in_the_live_walks_order() {
    let dir = init_repo();
    let root = dir.path();
    commit_authored_at(root, "a.rs", "1", "t", "t@e.x", "c1", "2025-01-01T00:00:00");
    let (index, _) = sync(root);
    drop(index);
    commit_authored_at(root, "a.rs", "2", "t", "t@e.x", "c2", "2025-01-02T00:00:00");
    commit_authored_at(root, "a.rs", "3", "t", "t@e.x", "c3", "2025-01-03T00:00:00");

    let (index, outcome) = sync(root);

    assert_eq!(outcome, RebuildOutcome::Incremental { added: 2 });
    assert!(index.tip_is_ordered(&basemind::git_history::keys::sha_hex_to_raw(&head_of(root)).expect("sha")));
    assert_eq!(index_recent(&index, &head_of(root)), live_walk(root));
    assert_eq!(live_walk(root).len(), 3);
}

/// Append order is not walk order once a head merges commits indexed for two other heads: the merge
/// is appended last, yet the walk lists the older side's commit after the newer one's. Such a head is
/// answered by the live walk (the index declines it), while the tips it joins stay served.
#[test]
fn a_merged_head_whose_order_the_index_cannot_prove_is_left_to_the_live_walk() {
    let dir = init_repo();
    let root = dir.path();
    commit_authored_at(root, "a.rs", "1", "t", "t@e.x", "c1", "2025-01-01T00:00:00");
    commit_authored_at(root, "a.rs", "2", "t", "t@e.x", "c2", "2025-01-03T00:00:00");
    let c2 = head_of(root);
    let (index, _) = sync(root);
    drop(index);
    run(root, &["checkout", "-qb", "feature", "HEAD~1"]);
    commit_authored_at(root, "b.rs", "1", "t", "t@e.x", "f1", "2025-01-02T00:00:00");
    let f1 = head_of(root);
    let (index, _) = sync(root);
    drop(index);
    run(root, &["checkout", "-q", "main"]);
    run(root, &["merge", "-q", "--no-ff", "feature", "-m", "merge"]);
    let merge = head_of(root);

    let (index, outcome) = sync(root);

    assert_eq!(outcome, RebuildOutcome::Incremental { added: 1 });
    assert!(index.has_tip(&merge), "the merge is an indexed tip");
    assert!(
        index.at(&merge).is_none(),
        "but its order is not provable from the index"
    );
    let c1 = head_of_rev(root, "HEAD~2");
    assert_eq!(
        live_walk(root),
        [merge, c2.clone(), f1.clone(), c1],
        "the live walk lists the newer side first, the order append order would invert"
    );
    for tip in [&c2, &f1] {
        assert!(index.at(tip).is_some(), "{tip} stays served");
    }
    assert_eq!(
        index_recent(&index, &c2),
        capture_lines(root, &["log", "--format=%H", &c2])
    );
    assert_eq!(
        index_recent(&index, &f1),
        capture_lines(root, &["log", "--format=%H", &f1])
    );
}

/// A head that branches off history indexed for another head is checked against its own walk: here
/// the walk descends through the ordinals, so the index serves it in the live walk's order.
#[test]
fn a_branch_off_indexed_history_whose_walk_descends_is_served_in_the_live_walks_order() {
    let dir = init_repo();
    let root = dir.path();
    commit_authored_at(root, "a.rs", "1", "t", "t@e.x", "c1", "2025-01-01T00:00:00");
    commit_authored_at(root, "a.rs", "2", "t", "t@e.x", "c2", "2025-01-02T00:00:00");
    let (index, _) = sync(root);
    drop(index);
    run(root, &["checkout", "-qb", "feature", "HEAD~1"]);
    commit_authored_at(root, "b.rs", "1", "t", "t@e.x", "f1", "2025-01-03T00:00:00");

    let (index, _) = sync(root);

    assert_eq!(index_recent(&index, &head_of(root)), live_walk(root));
}

fn head_of_rev(root: &Path, rev: &str) -> String {
    capture(root, &["rev-parse", rev]).trim().to_string()
}

fn capture_lines(root: &Path, args: &[&str]) -> Vec<String> {
    capture(root, args).lines().map(str::to_owned).collect()
}
