//! `by_path` paging on the live walk: what one page costs, what it leaves behind, and the cursors it
//! refuses.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use super::*;
use crate::git::FILE_LISTINGS;
use crate::mcp::cursor::Cursor;

/// Commits after the first: the odd ones touch `dir/`, the even ones a file beside it.
const COMMITS: usize = 6;
const WINDOW: u32 = 2;

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

/// A repository of `COMMITS + 1` commits, and a server state over it whose git cache persists to
/// the returned directory. No git-history index: every page is the live walk.
fn state(root: &Path, cache: &Path) -> ServerState {
    crate::store::init_isolated_cache();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("README"), "r\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    std::fs::create_dir_all(root.join("dir")).unwrap();
    for round in 1..=COMMITS {
        let file = if round % 2 == 1 {
            format!("dir/f{round}")
        } else {
            format!("top{round}")
        };
        std::fs::write(root.join(file), format!("{round}\n")).unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", &format!("round {round}")]);
    }
    let store = crate::store::Store::open(root, crate::store::VIEW_WORKING).expect("store");
    let repo = crate::git::Repo::discover(root).expect("repo");
    let shared = super::super::SharedReadStack::new(
        store,
        root.to_path_buf(),
        Arc::new(crate::config::Config::with_defaults()),
        Some(Arc::new(repo)),
        Arc::new(crate::git_cache::GitCache::open(cache, 4, true).expect("git cache")),
        None,
        super::super::ServerOptions::default(),
        #[cfg(all(feature = "comms", any(unix, windows)))]
        None,
    );
    ServerState::for_connection(Arc::new(shared), "test".into())
}

fn params(window: Option<u32>, limit: Option<u32>, cursor: Option<Cursor>) -> FindCommitsByPathParams {
    FindCommitsByPathParams {
        pattern: "^dir/".to_string(),
        window,
        limit,
        cursor,
        max_tokens: None,
    }
}

fn files_under(dir: &Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .map(|entry| {
                let path = entry.path();
                if path.is_dir() { files_under(&path) } else { 1 }
            })
            .sum()
    })
}

/// A page walks to its window without listing the files of the commits before it, and keeps no log
/// per page: a later page costs what the first does, and the disk cache does not grow with paging.
#[test]
fn a_later_page_lists_only_its_own_window_and_caches_nothing() {
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let state = state(repo_dir.path(), cache_dir.path());

    let mut cursor = None;
    let mut matched = Vec::new();
    let mut pages = 0;
    loop {
        FILE_LISTINGS.with(|listed| listed.set(0));
        let page = by_path(&state, "git_by_path", params(Some(WINDOW), None, cursor.take())).expect("page");
        pages += 1;
        let listed = FILE_LISTINGS.with(std::cell::Cell::get);
        assert!(
            listed <= WINDOW as usize + 1,
            "page {pages} listed the files of {listed} commits; its window is {WINDOW} plus one"
        );
        matched.extend(page.response.commits.iter().map(|commit| commit.summary.clone()));
        match page.response.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
        assert!(pages <= COMMITS + 1, "paging must end");
    }
    assert_eq!(pages, (COMMITS + 1).div_ceil(WINDOW as usize), "one page per window");
    assert_eq!(matched, ["round 5", "round 3", "round 1"], "every match, newest first");
    assert_eq!(files_under(cache_dir.path()), 0, "no log is written to disk per page");
}

/// A window or page of 0 would hand back the offset it was given, so a caller following
/// `next_cursor` would never stop.
#[test]
fn a_zero_window_or_page_is_refused() {
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let state = state(repo_dir.path(), cache_dir.path());

    let window = by_path(&state, "git_by_path", params(Some(0), None, None))
        .err()
        .expect("window 0");
    assert!(
        window.message.contains("`window` must be at least 1"),
        "{}",
        window.message
    );
    let limit = by_path(&state, "git_by_path", params(None, Some(0), None))
        .err()
        .expect("limit 0");
    assert!(
        limit.message.contains("`limit` must be at least 1"),
        "{}",
        limit.message
    );
}

/// A cursor counts in the list it was issued on. One from the git-history index is refused by the
/// live walk, and the other way round, with the reason and the way out.
#[test]
fn a_cursor_from_the_other_walk_is_refused_with_a_restart() {
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let state = state(repo_dir.path(), cache_dir.path());

    let first = by_path(&state, "git_log", params(Some(WINDOW), None, None)).expect("first page");
    let issued = first.response.next_cursor.expect("history goes on");
    let (offset, snapshot) = issued.decode_in_memory().expect("in-memory cursor");
    assert_eq!(snapshot & LIVE_WALK, LIVE_WALK, "issued on the live walk");
    by_path(&state, "git_log", params(Some(WINDOW), None, Some(issued))).expect("the live walk resumes it");

    let from_index = Cursor::encode_in_memory(offset, snapshot & !LIVE_WALK);
    let refused = by_path(&state, "git_log", params(Some(WINDOW), None, Some(from_index)))
        .err()
        .expect("an index cursor on the live walk");
    assert!(
        refused.message.contains("git-history index") && refused.message.contains("without `cursor`"),
        "{}",
        refused.message
    );
}
