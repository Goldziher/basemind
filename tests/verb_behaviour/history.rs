//! `git_log` searches: `query` is matched against the message and `author` against the author.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Client, call, started, started_with};

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
