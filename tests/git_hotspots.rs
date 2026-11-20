//! `basemind git hotspots` end to end (#134): a repository with dated commits, scanned, then ranked
//! by the one-shot CLI. The window is anchored at HEAD's commit time, so the fixture's dates fix
//! the answer whatever day the test runs.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

/// HEAD's commit time; every other date is days before it.
const HEAD_TIME: i64 = 1_750_000_000;
const DAY: i64 = 86_400;

/// Unix seconds `days` before HEAD.
fn days_before_head(days: i64) -> i64 {
    HEAD_TIME - days * DAY
}

/// Run `git args` in `root` as `email`, with the author date `authored` and the commit date
/// `committed` (unix seconds).
fn git_dated(root: &Path, args: &[&str], email: &str, authored: i64, committed: i64) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", email)
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_AUTHOR_DATE", format!("@{authored} +0000"))
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_DATE", format!("@{committed} +0000"))
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

fn git(root: &Path, args: &[&str], email: &str, days: i64) {
    let date = days_before_head(days);
    git_dated(root, args, email, date, date);
}

/// Write `files` and commit them as `email`, authored at `authored` and made at `committed`.
fn commit_dated(root: &Path, files: &[(&str, String)], email: &str, authored: i64, committed: i64) {
    for (path, text) in files {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }
    git_dated(root, &["add", "-A"], email, authored, committed);
    git_dated(root, &["commit", "-qm", "change"], email, authored, committed);
}

fn commit(root: &Path, files: &[(&str, String)], email: &str, days: i64) {
    let date = days_before_head(days);
    commit_dated(root, files, email, date, date);
}

/// A function `name` spanning `lines` lines.
fn function(name: &str, lines: usize) -> String {
    let mut text = format!("pub fn {name}() {{\n");
    for i in 0..lines.saturating_sub(2) {
        text.push_str(&format!("    let _v{i} = {i};\n"));
    }
    text.push_str("}\n");
    text
}

/// `hot.rs` is changed six times by two authors and holds the longest function; `big.rs` is the
/// largest file but changed once; ten one-function files under `sub/` are changed once each;
/// `old.rs` is changed eight times, all before the 30-day window.
fn build_and_scan() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"], "a@x", 400);
    git(root, &["config", "commit.gpgsign", "false"], "a@x", 400);
    commit(root, &[("README", "r\n".to_string())], "a@x", 400);
    for round in 0..8 {
        commit(
            root,
            &[("old.rs", function("old", 10 + round))],
            "a@x",
            300 - round as i64,
        );
    }
    for i in 0..10 {
        commit(
            root,
            &[(&format!("sub/s{i}.rs"), function(&format!("s{i}"), 3))],
            "a@x",
            20,
        );
    }
    commit(root, &[("big.rs", "// filler\n".repeat(500))], "a@x", 15);
    // The last round is HEAD, at exactly `HEAD_TIME`.
    for round in 0..6 {
        let email = if round % 2 == 0 { "a@x" } else { "b@x" };
        commit(root, &[("hot.rs", hot_text(round))], email, 5 - round as i64);
    }
    scan(root);
    dir
}

fn scan(root: &Path) {
    let status = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["--root", root.to_str().unwrap(), "scan", "--quiet"])
        .status()
        .expect("run basemind scan");
    assert!(status.success(), "basemind scan failed");
}

/// `hot.rs`'s content in `round`: three functions, the first `50 + round` lines long.
fn hot_text(round: usize) -> String {
    format!(
        "{}{}{}",
        function("long", 50 + round),
        function("short", 3),
        function("other", 4)
    )
}

/// Thirty commits in the 30-day window (ten one-function files under `sub/`, then `hot.rs` twenty
/// times by two authors), a branch of sixty commits to `old.rs` authored 200 days ago but made
/// after them, and HEAD, the merge of that branch, authored a day before it was made.
fn build_merged_branch_and_scan() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"], "a@x", 400);
    git(root, &["config", "commit.gpgsign", "false"], "a@x", 400);
    commit(root, &[("README", "r\n".to_string())], "a@x", 400);
    git(root, &["branch", "old"], "a@x", 400);
    for i in 0..10 {
        commit(
            root,
            &[(&format!("sub/s{i}.rs"), function(&format!("s{i}"), 3))],
            "a@x",
            25,
        );
    }
    for round in 0..20 {
        let email = if round % 2 == 0 { "a@x" } else { "b@x" };
        commit(root, &[("hot.rs", hot_text(round))], email, 24 - round as i64);
    }
    git(root, &["checkout", "-q", "old"], "a@x", 3);
    for i in 0..60 {
        commit_dated(
            root,
            &[("old.rs", format!("pub const V: u32 = {i};\n"))],
            "c@x",
            days_before_head(200) + i,
            days_before_head(3) + i,
        );
    }
    git(root, &["checkout", "-q", "-"], "a@x", 3);
    git_dated(
        root,
        &["merge", "-q", "--no-ff", "--no-edit", "old"],
        "a@x",
        days_before_head(1),
        HEAD_TIME,
    );
    scan(root);
    dir
}

fn hotspots(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(["--root", root.to_str().unwrap(), "--json", "git", "hotspots"])
        .args(args)
        .output()
        .expect("run basemind git hotspots")
}

fn json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "git hotspots failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&output.stdout)))
}

fn paths(response: &Value) -> Vec<&str> {
    response["files"]
        .as_array()
        .unwrap_or_else(|| panic!("files: {response}"))
        .iter()
        .map(|row| row["path"].as_str().unwrap())
        .collect()
}

#[test]
fn hotspots_rank_the_files_where_change_and_size_coincide() {
    let dir = build_and_scan();
    let root = dir.path();

    let response = json(&hotspots(root, &["--since", "30d"]));
    assert_eq!(response["window_end"], HEAD_TIME, "{response}");
    assert_eq!(response["window_start"], HEAD_TIME - 30 * DAY, "{response}");
    // Ten `sub/` files, big.rs and hot.rs: old.rs changed only before the window.
    assert_eq!(response["files_considered"], 12, "{response}");
    assert_eq!(response["commits"], 17, "{response}");
    assert_eq!(paths(&response), ["hot.rs"], "{response}");
    let hot = &response["files"][0];
    assert_eq!(hot["commits"], 6, "{response}");
    assert_eq!(hot["authors"], 2, "{response}");
    assert_eq!(hot["lines"], 55 + 3 + 4, "{response}");
    assert_eq!(hot["symbols"], 3, "{response}");
    assert_eq!(hot["longest_function"], 55, "{response}");
    assert_eq!(hot["longest_function_at"], "long:1", "{response}");
    let signals: Vec<&str> = hot["signals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert_eq!(
        signals,
        ["commits", "authors", "lines", "symbols", "longest_function"],
        "{response}"
    );
    // big.rs is the largest file but in the top slice of `lines` only.
    assert_eq!(response["thresholds"]["lines"], 62, "{response}");

    // The whole history: old.rs is ranked too, the most changed and with the second longest
    // function, which puts it after hot.rs's five coinciding signals.
    let all = json(&hotspots(root, &["--since", "2y"]));
    assert_eq!(all["files_considered"], 13, "{all}");
    assert_eq!(all["thresholds"]["commits"], 6, "{all}");
    assert_eq!(paths(&all), ["hot.rs", "old.rs"], "{all}");
    assert_eq!(
        all["files"][1]["signals"],
        serde_json::json!(["commits", "longest_function"]),
        "{all}"
    );
    assert!(
        all["notes"]
            .as_array()
            .is_none_or(|notes| notes.iter().all(|note| !note.as_str().unwrap().contains("partial"))),
        "no resolver, so the graph is not partial: {all}"
    );

    // Scoped to `sub/`: one change each, nothing coincides.
    let scoped = json(&hotspots(root, &["--since", "30d", "--path", "sub"]));
    assert_eq!(scoped["files_considered"], 10, "{scoped}");
    assert_eq!(scoped["total"], 0, "{scoped}");

    let refused = hotspots(root, &["--since", "6m"]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("`90d`, `12w` or `1y`"), "{stderr}");
}

#[test]
fn a_merged_branch_authored_before_the_window_does_not_end_it() {
    let dir = build_merged_branch_and_scan();
    let response = json(&hotspots(dir.path(), &["--since", "30d"]));
    assert_eq!(response["window_end"], HEAD_TIME, "{response}");
    assert_eq!(response["window_start"], HEAD_TIME - 30 * DAY, "{response}");
    // The merge and the thirty commits before the branch; the branch's are authored before the
    // window, so `old.rs` is not ranked.
    assert_eq!(response["commits"], 31, "{response}");
    assert_eq!(response["files_considered"], 11, "{response}");
    assert_eq!(response["truncated"], Value::Null, "{response}");
    assert_eq!(response["truncated_reason"], Value::Null, "{response}");
    assert_eq!(paths(&response), ["hot.rs"], "{response}");
    let hot = &response["files"][0];
    assert_eq!(hot["commits"], 20, "{response}");
    assert_eq!(hot["authors"], 2, "{response}");
    assert_eq!(hot["lines"], 69 + 3 + 4, "{response}");
    assert_eq!(hot["symbols"], 3, "{response}");
    assert_eq!(hot["longest_function"], 69, "{response}");
    assert_eq!(hot["longest_function_at"], "long:1", "{response}");
    assert_eq!(
        hot["signals"],
        serde_json::json!(["commits", "authors", "lines", "symbols", "longest_function"]),
        "{response}"
    );
}
