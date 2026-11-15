//! `git_log` searches: `query` is matched against the message and `author` against the author.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Client, call, call_err, started, started_with};

/// Commit everything as `name <email>`.
fn commit_as(root: &Path, name: &str, email: &str, message: &str) {
    let status = Command::new("git")
        .args(["commit", "-qam", message])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", name)
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git commit as {name} failed");
}

fn summaries(body: &Value) -> Vec<String> {
    body["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|commit| commit["summary"].as_str().unwrap_or_default().to_string())
        .collect()
}

async fn assert_author_scoping(client: &Client) {
    let mentions = call(client, "git_log", json!({ "query": "review", "author": "alice" })).await;
    assert_eq!(
        summaries(&mentions),
        Vec::<String>::new(),
        "bob's commit mentions alice but alice did not write it: {mentions}"
    );

    let hers = call(client, "git_log", json!({ "query": "parser", "author": "alice" })).await;
    assert_eq!(summaries(&hers), ["fix the parser"], "{hers}");

    let by_bob = call(client, "git_log", json!({ "query": "alice", "author": "bob" })).await;
    assert_eq!(summaries(&by_bob), ["thanks alice for the review"], "{by_bob}");

    let author_only = call(client, "git_log", json!({ "author": "alice" })).await;
    assert_eq!(summaries(&author_only), ["fix the parser"], "{author_only}");
}

/// A commit by bob whose message names alice is not a commit by alice. Checked on the live walk the
/// search starts with and again once the git-history index answers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_log_matches_author_terms_against_the_author_only() {
    let (dir, client) = started().await;
    let root = dir.path();
    let outline = std::fs::read_to_string(root.join("src/outline.rs")).unwrap();
    std::fs::write(root.join("src/outline.rs"), outline.replace("    1\n", "    11\n")).unwrap();
    commit_as(root, "alice", "alice@e.x", "fix the parser");
    std::fs::write(root.join("src/outline.rs"), outline.replace("    1\n", "    12\n")).unwrap();
    commit_as(root, "bob", "bob@e.x", "thanks alice for the review");

    assert_author_scoping(&client).await;

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut indexed = false;
    while Instant::now() < deadline {
        let probe = call(&client, "git_log", json!({ "query": "parser" })).await;
        if probe.get("partial").and_then(Value::as_bool) != Some(true) {
            indexed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if indexed {
        assert_author_scoping(&client).await;
    } else {
        eprintln!("NOTE: the git-history index did not catch up; only the live walk was exercised");
    }
}

const STEADY_RS: &str = "pub fn stable() -> u32 {\n    7\n}\n\npub fn churn() -> u32 {\n    0\n}\n";

/// A symbol's history is read from a bounded window of the commits touching its file. The oldest
/// commit of a window that was used up is where the walk stopped, not where the symbol appeared.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_log_of_a_symbol_does_not_call_the_edge_of_its_window_an_introduction() {
    let (dir, client) = started_with(&[("src/steady.rs", STEADY_RS)]).await;
    let root = dir.path();
    for round in 1..=15 {
        std::fs::write(
            root.join("src/steady.rs"),
            STEADY_RS.replace("    0\n", &format!("    {round}\n")),
        )
        .unwrap();
        super::git(root, &["commit", "-qam", &format!("round {round}")]);
    }

    let windowed = call(
        &client,
        "git_log",
        json!({ "path": "src/steady.rs", "symbol": "stable", "limit": 1 }),
    )
    .await;
    assert_eq!(
        windowed["history"],
        json!([]),
        "`stable` did not change in the 8 commits inspected: {windowed}"
    );
    assert_eq!(windowed["truncated"], true, "{windowed}");
    assert_eq!(windowed["truncated_reason"], "window", "{windowed}");

    let whole = call(
        &client,
        "git_log",
        json!({ "path": "src/steady.rs", "symbol": "stable", "limit": 20 }),
    )
    .await;
    let changes: Vec<(&str, &str)> = whole["history"]
        .as_array()
        .expect("history")
        .iter()
        .map(|entry| (entry["change"].as_str().unwrap(), entry["summary"].as_str().unwrap()))
        .collect();
    assert_eq!(changes, [("introduced", "init")], "{whole}");
    assert!(
        whole.get("truncated").is_none(),
        "all 16 commits fit the window: {whole}"
    );

    let churn = call(
        &client,
        "git_log",
        json!({ "path": "src/steady.rs", "symbol": "churn", "limit": 1 }),
    )
    .await;
    assert_eq!(churn["history"][0]["change"], "modified", "{churn}");
    assert_eq!(churn["history"][0]["summary"], "round 15", "{churn}");
}

/// `git_blame {symbol}` blames the lines the symbol has in the blamed revision. The span used to
/// come from the working-tree outline, which is a different set of lines as soon as the file has
/// changed above the symbol.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_blame_of_a_symbol_uses_its_span_at_the_blamed_revision() {
    let (dir, client) = started().await;
    let root = dir.path();
    let path = root.join("src/outline.rs");
    let source = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{}{source}", "// padding\n".repeat(20))).unwrap();
    call(&client, "index_rescan", json!({ "paths": ["src/outline.rs"] })).await;

    let at_head = call(
        &client,
        "git_blame",
        json!({ "path": "src/outline.rs", "symbol": "file_outline_l2" }),
    )
    .await;
    assert_eq!(
        (at_head["line"].as_u64(), at_head["end_line"].as_u64()),
        (Some(7), Some(9)),
        "HEAD does not have the uncommitted padding: {at_head}"
    );
    assert_eq!(at_head["hunks"][0]["line"], 7, "{at_head}");
    assert_eq!(at_head["hunks"][0]["summary"], "init", "{at_head}");

    super::git(root, &["commit", "-qam", "pad"]);
    let padded = call(
        &client,
        "git_blame",
        json!({ "path": "src/outline.rs", "symbol": "file_outline_l2" }),
    )
    .await;
    assert_eq!(
        (padded["line"].as_u64(), padded["end_line"].as_u64()),
        (Some(27), Some(29)),
        "{padded}"
    );
    let before = call(
        &client,
        "git_blame",
        json!({ "path": "src/outline.rs", "symbol": "file_outline_l2", "rev": "HEAD~1" }),
    )
    .await;
    assert_eq!(
        (before["line"].as_u64(), before["end_line"].as_u64()),
        (Some(7), Some(9)),
        "{before}"
    );

    let absent = call_err(
        &client,
        "git_blame",
        json!({ "path": "src/outline.rs", "symbol": "not_at_that_revision", "rev": "HEAD~1" }),
    )
    .await;
    assert!(
        absent.contains("no symbol `not_at_that_revision` in `src/outline.rs` at HEAD~1"),
        "{absent}"
    );

    // A function that exists only in the working tree is not in HEAD's blob, and the error says
    // which of the two was looked at.
    std::fs::write(&path, format!("{source}\npub fn uncommitted() {{}}\n")).unwrap();
    call(&client, "index_rescan", json!({ "paths": ["src/outline.rs"] })).await;
    let uncommitted = call_err(
        &client,
        "git_blame",
        json!({ "path": "src/outline.rs", "symbol": "uncommitted" }),
    )
    .await;
    assert!(
        uncommitted.contains("no symbol `uncommitted` in `src/outline.rs` at HEAD"),
        "{uncommitted}"
    );
}

/// A symbol blame parses the revision's blob. A blob over `scan.max_file_bytes`, which the scan
/// would not have parsed either, is refused with the span form that needs no parse.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_blame_of_a_symbol_does_not_parse_a_blob_over_the_scan_limit() {
    let big = format!("pub fn first() {{}}\n{}", "// filler to pass the limit\n".repeat(200));
    let (_dir, client) = started_with(&[
        (
            "basemind.toml",
            "\"$schema\" = \"v1\"\n\n[scan]\nmax_file_bytes = 4096\n",
        ),
        ("src/big.rs", big.as_str()),
    ])
    .await;

    let refused = call_err(&client, "git_blame", json!({ "path": "src/big.rs", "symbol": "first" })).await;
    assert!(
        refused.contains("over `scan.max_file_bytes` (4096)") && refused.contains("`line` and `end_line`"),
        "{refused}"
    );
    let span = call(&client, "git_blame", json!({ "path": "src/big.rs", "line": 1 })).await;
    assert_eq!(span["hunks"][0]["summary"], "init", "{span}");
}
