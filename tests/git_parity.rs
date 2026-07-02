//! Precision parity of the git-history index against **real git**, on a real repository, at full
//! branch depth. The deterministic synthetic cases live in `git_history_smoke.rs`; this harness
//! points at a large real repo to catch precision bugs that only surface at scale (deep author
//! history, thousands of commits, real tokenization).
//!
//! `#[ignore]`-gated like `harden.rs` — run explicitly:
//!
//! ```bash
//! BASEMIND_GIT_PARITY_REPO=/abs/path/to/repo \
//!   cargo test --release --test git_parity -- --ignored --nocapture
//! ```
//!
//! With no `BASEMIND_GIT_PARITY_REPO` set the test skips (prints a note) so it stays a no-op in
//! environments without a target repo. The target repo is referenced **only** through this env var
//! — never hardcoded — so no specific repository name lives in the tree.
//!
//! Every git oracle is pinned to the exact sha the index built from (`last_indexed_head_hex`), not
//! live `HEAD` — the target may be an actively-committed working repo, and the index build takes a
//! minute-plus, so an unpinned `git log HEAD` would race new commits and diverge spuriously.
//!
//! Scope: HEAD/current-branch, matching plain `git log <sha>` (NOT `git log --all`).

use std::path::{Path, PathBuf};
use std::process::Command;

use basemind::git::Repo;
use basemind::git_history::GitHistoryIndex;
use basemind::git_history::builder;
use basemind::git_history::fts::FtsScope;

/// `git -C <repo> <args>` → trimmed stdout lines (drops the trailing empty line).
fn git_lines(repo: &Path, args: &[&str]) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git in PATH");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .lines()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .collect()
}

fn git_one(repo: &Path, args: &[&str]) -> Option<String> {
    git_lines(repo, args).into_iter().next()
}

/// The repo under test, or `None` to skip.
fn target_repo() -> Option<PathBuf> {
    let raw = std::env::var("BASEMIND_GIT_PARITY_REPO").ok()?;
    let path = PathBuf::from(raw);
    assert!(
        path.join(".git").exists(),
        "BASEMIND_GIT_PARITY_REPO={} is not a git repo (no .git)",
        path.display()
    );
    Some(path)
}

/// Build a fresh git-history index for `repo` in a throwaway `.basemind/` (never touches the repo's
/// real cache), synced to HEAD, and return it with the exact sha it indexed. All git oracles use
/// that sha so a commit landing mid-build can't race the comparison.
fn build_index(repo: &Path, scratch: &Path) -> (GitHistoryIndex, String) {
    let repository = Repo::discover(repo).expect("discover repo");
    let index = GitHistoryIndex::open(scratch).expect("open git-history index");
    let outcome = builder::sync(&index, &repository, scratch).expect("sync git-history index");
    let head = index
        .last_indexed_head_hex()
        .expect("index records the head it built from");
    eprintln!(
        "git_parity: built index — {outcome:?}, {} commits, head {}",
        index.commit_count(),
        &head[..12.min(head.len())]
    );
    (index, head)
}

#[test]
#[ignore = "requires BASEMIND_GIT_PARITY_REPO to point at a real repo"]
fn author_search_matches_real_git_at_full_depth() {
    let Some(repo) = target_repo() else {
        eprintln!("git_parity: BASEMIND_GIT_PARITY_REPO unset — skipping");
        return;
    };
    let scratch = tempfile::tempdir().expect("tempdir");
    let (index, head) = build_index(&repo, scratch.path());

    // Pick a real author deep in history: the author of the 250th commit back (well outside any
    // recent window). Falls back to the tip's author on a shallow repo. Pinned to the index head.
    let deep = git_one(&repo, &["log", &head, "--format=%an", "-1", "--skip=250"])
        .or_else(|| git_one(&repo, &["log", &head, "--format=%an", "-1"]))
        .expect("at least one commit");
    eprintln!("git_parity: author under test = {deep:?}");

    // Oracle: git's own case-insensitive author search over the pinned head's history.
    let git_shas = git_lines(
        &repo,
        &[
            "log",
            &head,
            "-i",
            &format!("--author={deep}"),
            "--format=%H",
        ],
    );
    assert!(!git_shas.is_empty(), "git found no commits for {deep:?}");
    let git_newest = &git_shas[0];
    let git_set: std::collections::HashSet<&String> = git_shas.iter().collect();

    // Index: full-depth author search, newest-first.
    let hits = index.search_commits(&deep, FtsScope::Author, 0, 5000);
    assert!(
        !hits.is_empty(),
        "index author search returned nothing for {deep:?} (git found {})",
        git_shas.len()
    );

    // 1. The newest hit is the author's most recent commit — the "what did <author> do last" answer.
    assert_eq!(
        &hits[0].sha, git_newest,
        "index newest author commit must equal `git log -i --author` newest"
    );

    // 2. No false positives: every indexed hit is a real reachable commit by that author.
    //    (git's `--author` is a substring/regex match; token-AND is a subset of it, so ⊆ must hold.)
    for h in &hits {
        assert!(
            git_set.contains(&h.sha),
            "indexed author hit {} is not in git's author set for {deep:?}",
            h.sha
        );
    }
    eprintln!(
        "git_parity: author {deep:?} — git {} commits, index returned {} (newest matches)",
        git_shas.len(),
        hits.len()
    );
}

#[test]
#[ignore = "requires BASEMIND_GIT_PARITY_REPO to point at a real repo"]
fn recent_and_path_history_match_real_git() {
    let Some(repo) = target_repo() else {
        eprintln!("git_parity: BASEMIND_GIT_PARITY_REPO unset — skipping");
        return;
    };
    let scratch = tempfile::tempdir().expect("tempdir");
    let (index, head) = build_index(&repo, scratch.path());

    // recent_changes parity: newest 50 commits, newest-first, pinned to the index head.
    let git_recent = git_lines(&repo, &["log", &head, "--format=%H", "-50"]);
    let idx_recent: Vec<String> = index
        .recent_commits(0, git_recent.len(), false)
        .into_iter()
        .map(|c| c.sha)
        .collect();
    assert_eq!(
        idx_recent, git_recent,
        "recent_commits must match `git log <head>` newest-first, exactly"
    );

    // commits_touching parity for a real tracked path: use a file changed in the head commit.
    if let Some(path) = git_one(&repo, &["show", "--format=", "--name-only", &head]) {
        let git_touch = git_lines(
            &repo,
            &["log", &head, "--full-history", "--format=%H", "--", &path],
        );
        let idx_touch: Vec<String> = index
            .commits_touching(&path.as_bytes().into(), 0, git_touch.len())
            .into_iter()
            .map(|c| c.sha)
            .collect();
        assert_eq!(
            idx_touch, git_touch,
            "commits_touching({path}) must match `git log --full-history -- <path>`"
        );
        eprintln!(
            "git_parity: commits_touching({path}) — {} commits match",
            git_touch.len()
        );
    }
}
