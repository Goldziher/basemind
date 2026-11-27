//! A one-shot `basemind scan` of the working tree leaves a from-scratch git-history build to the
//! background worker and returns (#518); an append, or a scan with the worker turned off, builds
//! the index before it returns.
//!
//! Drives the built binary under the process-wide isolated data home, with its own comms dir and
//! no daemon autospawn, so no daemon of the machine is reached.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

#[cfg(all(feature = "comms", any(unix, windows)))]
use basemind::git_history::progress;
use basemind::git_history::{GitHistoryIndex, shared_history_basemind_dir};

fn git(root: &Path, args: &[&str]) -> String {
    let out = basemind::testing::git_command()
        .args(args)
        .current_dir(root)
        .output()
        .expect("git in PATH");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout).expect("utf8")
}

fn commit(root: &Path, file: &str) {
    std::fs::write(
        root.join(file),
        format!("pub fn {}() {{}}\n", file.trim_end_matches(".rs")),
    )
    .expect("write");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", file]);
}

/// A repository with one commit per file of `files`.
fn repository(files: &[&str]) -> tempfile::TempDir {
    basemind::store::init_isolated_cache();
    let repo = tempfile::tempdir().expect("repo");
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    for file in files {
        commit(repo.path(), file);
    }
    repo
}

/// `basemind --json scan` of `root`, with the background worker on unless `background` is false.
fn scan(root: &Path, comms_dir: &Path, background: bool) -> serde_json::Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_basemind"));
    command
        .args(["--root", root.to_str().expect("utf-8 path"), "--json", "scan"])
        .env("BASEMIND_COMMS_DIR", comms_dir)
        .env("BASEMIND_NO_AUTOSPAWN", "1");
    if !background {
        command.env("BASEMIND_EMBED_BACKGROUND", "false");
    }
    let output = command.output().expect("run basemind scan");
    assert!(
        output.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("the scan answers JSON")
}

/// The git-history index of `root` once no process holds it, waiting up to `limit`.
fn released_index(root: &Path, limit: Duration) -> GitHistoryIndex {
    let deadline = Instant::now() + limit;
    loop {
        match GitHistoryIndex::open_once(&shared_history_basemind_dir(root)) {
            Ok(index) => return index,
            Err(error) => {
                assert!(Instant::now() < deadline, "the index stayed held: {error}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn head(root: &Path) -> String {
    git(root, &["rev-parse", "HEAD"]).trim().to_string()
}

#[cfg(all(feature = "comms", any(unix, windows)))]
#[test]
fn a_first_scan_hands_the_history_build_to_the_background_and_returns() {
    let repo = repository(&["a.rs", "b.rs"]);
    let root = repo.path();
    let comms_dir = tempfile::tempdir().expect("comms dir");

    let answer = scan(root, comms_dir.path(), true);
    assert_eq!(
        answer["git_history"],
        serde_json::json!({"pending_commits": 2, "background": "started"}),
        "{answer}"
    );

    let history_dir = shared_history_basemind_dir(root);
    let deadline = Instant::now() + Duration::from_secs(120);
    while progress::is_owed(&history_dir) {
        assert!(Instant::now() < deadline, "the background build never finished");
        std::thread::sleep(Duration::from_millis(100));
    }
    let index = released_index(root, Duration::from_secs(60));
    assert!(index.has_tip(&head(root)), "the worker built the index at HEAD");
    assert_eq!(index.commit_count(), 2);
}

#[test]
fn with_the_worker_off_or_only_an_append_owed_the_scan_builds_the_index_itself() {
    let repo = repository(&["a.rs"]);
    let root = repo.path();
    let comms_dir = tempfile::tempdir().expect("comms dir");

    let answer = scan(root, comms_dir.path(), false);
    assert!(answer.get("git_history").is_none(), "{answer}");
    let index = released_index(root, Duration::from_secs(5));
    assert!(index.has_tip(&head(root)), "built before the scan returned");
    drop(index);

    commit(root, "b.rs");
    let answer = scan(root, comms_dir.path(), true);
    assert!(
        answer.get("git_history").is_none(),
        "an append is not handed off: {answer}"
    );
    let index = released_index(root, Duration::from_secs(5));
    assert!(index.has_tip(&head(root)), "the append ran before the scan returned");
    assert_eq!(index.commit_count(), 2);
}
