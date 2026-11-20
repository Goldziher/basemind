//! `git_log` with a file `path`: a fresh git-history index answers a path no commit touches without
//! walking the history, and the live walk tests a bounded span of commits per page.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use super::*;
use crate::git::TOUCHING_TESTS;
use crate::git_history::GitHistoryIndex;

/// Commits after the first, each changing `a.rs`.
const COMMITS: usize = 40;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .output()
        .expect("git in PATH");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout).expect("utf8")
}

/// A repository whose first commit adds `rare.rs` and `a.rs` and whose `commits` later ones each
/// change `a.rs`, and its HEAD.
fn repository(root: &Path, commits: usize) -> String {
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("rare.rs"), "fn rare() {}\n").unwrap();
    std::fs::write(root.join("a.rs"), "0\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    for round in 1..=commits {
        std::fs::write(root.join("a.rs"), format!("{round}\n")).unwrap();
        git(root, &["commit", "-qam", &format!("round {round}")]);
    }
    git(root, &["rev-parse", "HEAD"]).trim().to_string()
}

/// A workspace over the repository at `root`, with `git_history` as its git-history index.
fn workspace_over(root: &Path, cache: &Path, git_history: Option<Arc<GitHistoryIndex>>) -> Workspace {
    crate::store::init_isolated_cache();
    let store = crate::store::Store::open(root, crate::store::VIEW_WORKING).expect("store");
    let repo = crate::git::Repo::discover(root).expect("repo");
    Workspace::new(
        store,
        root.to_path_buf(),
        Arc::new(crate::config::Config::with_defaults()),
        Some(Arc::new(repo)),
        Arc::new(crate::git_cache::GitCache::open(cache, 4, true).expect("git cache")),
        git_history,
        crate::engine::workspace::OpenOptions {
            background: true,
            read_only: false,
            forwards_writes: false,
            lazy_cache: false,
            history_sync: None,
            #[cfg(feature = "memory")]
            memory_owner: |_| "owner".to_string(),
        },
    )
}

fn touching(path: &str, limit: u32) -> CommitsTouchingParams {
    CommitsTouchingParams {
        path: RelPath::from(path.as_bytes()),
        limit: Some(limit),
        cursor: None,
        max_tokens: None,
    }
}

/// Commits `run_touching` tested against its path.
fn tested_by(call: impl FnOnce()) -> usize {
    TOUCHING_TESTS.with(|tests| tests.set(0));
    call();
    TOUCHING_TESTS.with(std::cell::Cell::get)
}

/// An empty answer from an index that holds HEAD is the answer: a path no commit touches costs no
/// walk of the history, on disk or not.
#[test]
fn a_fresh_index_answers_a_path_no_commit_touches_without_a_walk() {
    let repo_dir = tempfile::tempdir().unwrap();
    let root = repo_dir.path();
    let head = repository(root, COMMITS);
    let history_dir = tempfile::tempdir().unwrap();
    let index = GitHistoryIndex::open(history_dir.path()).expect("open the index");
    let repo = crate::git::Repo::discover(root).expect("repo");
    crate::git_history::builder::sync(&index, &repo, history_dir.path()).expect("build the index");
    assert_eq!(
        index.last_indexed_head_hex().as_deref(),
        Some(head.as_str()),
        "the index is fresh"
    );
    let cache_dir = tempfile::tempdir().unwrap();
    let ws = workspace_over(root, cache_dir.path(), Some(Arc::new(index)));

    let mut missing = None;
    let walked = tested_by(|| missing = Some(run_touching(&ws, touching("missing.rs", 5))));
    let refused = missing.expect("ran").expect_err("a path that is nowhere");
    assert_eq!(
        refused.to_string(),
        "`git_log`: no commit reachable from HEAD touches `missing.rs` and no such file exists. `path` is a \
         repo-relative file path, or a directory when it ends with `/`; locate it with `find_files`."
    );
    assert_eq!(walked, 0, "the index answered; no commit was walked");

    std::fs::write(root.join("untracked.rs"), "fn new() {}\n").unwrap();
    let mut untracked = None;
    let walked = tested_by(|| untracked = Some(run_touching(&ws, touching("untracked.rs", 5))));
    let page = untracked.expect("ran").expect("a file on disk no commit touches");
    assert_eq!(page.commits.len(), 0);
    assert_eq!(page.next_cursor, None);
    assert_eq!(page.truncated_reason, None);
    assert_eq!(walked, 0, "the index answered; no commit was walked");

    let mut tracked = None;
    let walked = tested_by(|| tracked = Some(run_touching(&ws, touching("rare.rs", 5))));
    let page = tracked.expect("ran").expect("a tracked file");
    let summaries: Vec<&str> = page.commits.iter().map(|commit| commit.summary.as_str()).collect();
    assert_eq!(summaries, ["init"]);
    assert_eq!(walked, 0, "the index answered; no commit was walked");
}

/// Without the index, one page tests at most its span of commits; a span that runs out before a
/// match hands back where the next page resumes, and the pages together find the match.
#[test]
fn the_live_walk_tests_one_span_per_page_and_resumes_where_it_ran_out() {
    const SPAN: usize = 4;
    let repo_dir = tempfile::tempdir().unwrap();
    let root = repo_dir.path();
    let head = repository(root, 9);
    let newest_first: Vec<String> = git(root, &["rev-list", "HEAD"]).lines().map(str::to_string).collect();
    assert_eq!(newest_first.len(), 10);
    let repo = crate::git::Repo::discover(root).expect("repo");
    let rare = RelPath::from("rare.rs");

    let mut pages = Vec::new();
    let mut from = 0;
    loop {
        let mut span = None;
        let walked = tested_by(|| span = Some(repo.touching_span(&head, &rare, from, 2, SPAN).expect("walk")));
        let span = span.expect("ran");
        assert!(walked <= SPAN, "page {} tested {walked} commits", pages.len() + 1);
        let next = span.resume.as_ref().map(|(position, _)| *position);
        pages.push(span);
        match next {
            Some(position) => from = position,
            None => break,
        }
    }
    assert_eq!(pages.len(), 3, "spans of 4, 4 and the last 2 commits");
    assert_eq!(pages[0].commits, Vec::new());
    assert_eq!(pages[0].resume, Some((4, newest_first[4].clone())));
    assert_eq!(pages[1].commits, Vec::new());
    assert_eq!(pages[1].resume, Some((8, newest_first[8].clone())));
    let found: Vec<(usize, &str)> = pages[2]
        .commits
        .iter()
        .map(|(position, commit)| (*position, commit.summary.as_str()))
        .collect();
    assert_eq!(found, [(9, "init")]);
    assert_eq!(pages[2].resume, None);

    let snapshot = (head_snapshot_id(&head) & !LIVE_WALK) | LIVE_WALK;
    let page = touching_response(pages.swap_remove(0), 5, None, snapshot, false);
    assert_eq!(page.commits.len(), 0);
    assert!(page.truncated);
    assert_eq!(page.truncated_reason, Some(SPAN_LIMIT));
    let next = page
        .next_cursor
        .expect("the history goes on")
        .decode_in_memory()
        .expect("cursor");
    assert_eq!(
        next,
        (4, snapshot),
        "the next page resumes at the first commit left untested"
    );
}

/// A span that runs out after a match lists it, and the next page still resumes past the span.
#[test]
fn a_span_that_ran_out_after_a_match_lists_it_and_resumes_past_the_span() {
    let repo_dir = tempfile::tempdir().unwrap();
    let root = repo_dir.path();
    let head = repository(root, 9);
    let newest_first: Vec<String> = git(root, &["rev-list", "HEAD"]).lines().map(str::to_string).collect();
    let repo = crate::git::Repo::discover(root).expect("repo");

    let span = repo
        .touching_span(&head, &RelPath::from("a.rs"), 0, 6, 3)
        .expect("walk");
    let found: Vec<(usize, &str)> = span
        .commits
        .iter()
        .map(|(position, commit)| (*position, commit.summary.as_str()))
        .collect();
    assert_eq!(found, [(0, "round 9"), (1, "round 8"), (2, "round 7")]);
    assert_eq!(span.resume, Some((3, newest_first[3].clone())));

    let snapshot = (head_snapshot_id(&head) & !LIVE_WALK) | LIVE_WALK;
    let page = touching_response(span, 5, None, snapshot, false);
    let summaries: Vec<&str> = page.commits.iter().map(|commit| commit.summary.as_str()).collect();
    assert_eq!(summaries, ["round 9", "round 8", "round 7"]);
    assert_eq!(page.truncated_reason, Some(SPAN_LIMIT));
    let next = page
        .next_cursor
        .expect("the history goes on")
        .decode_in_memory()
        .expect("cursor");
    assert_eq!(next, (3, snapshot));
}
