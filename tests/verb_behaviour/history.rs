//! `git_log` searches: `query` is matched against the message and `author` against the author.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{
    Client, blame_hunks, call, call_err, commit_summaries, fixture_with, history_change, scan, serve, started,
    started_with,
};

/// Commit everything as `name <email>`.
fn commit_as(root: &Path, name: &str, email: &str, message: &str) {
    let status = basemind::testing::git_command()
        .args(["commit", "-qam", message])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", name)
        .env("GIT_AUTHOR_EMAIL", email)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git commit as {name} failed");
}

/// A search by `author` writes each row `"<sha> <author>: <summary>"`; this is the part after the sha.
fn summaries(body: &Value) -> Vec<String> {
    commit_summaries(&body["commits"])
}

async fn assert_author_scoping(client: &Client) {
    let mentions = call(client, "git_log", json!({ "query": "review", "author": "alice" })).await;
    assert_eq!(
        summaries(&mentions),
        Vec::<String>::new(),
        "bob's commit mentions alice but alice did not write it: {mentions}"
    );

    let hers = call(client, "git_log", json!({ "query": "parser", "author": "alice" })).await;
    assert_eq!(summaries(&hers), ["alice: fix the parser"], "{hers}");

    let by_bob = call(client, "git_log", json!({ "query": "alice", "author": "bob" })).await;
    assert_eq!(summaries(&by_bob), ["bob: thanks alice for the review"], "{by_bob}");

    let author_only = call(client, "git_log", json!({ "author": "alice" })).await;
    assert_eq!(summaries(&author_only), ["alice: fix the parser"], "{author_only}");
}

/// A commit by bob whose message names alice is not a commit by alice. Checked on the live walk the
/// search starts with and again once the git-history index answers it. The commits are made before
/// the server starts, so the index it builds at start holds them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_log_matches_author_terms_against_the_author_only() {
    let dir = fixture_with(&[]);
    let root = dir.path();
    let outline = std::fs::read_to_string(root.join("src/outline.rs")).unwrap();
    std::fs::write(root.join("src/outline.rs"), outline.replace("    1\n", "    11\n")).unwrap();
    commit_as(root, "alice", "alice@e.x", "fix the parser");
    std::fs::write(root.join("src/outline.rs"), outline.replace("    1\n", "    12\n")).unwrap();
    commit_as(root, "bob", "bob@e.x", "thanks alice for the review");
    scan(root);
    let client = serve(root).await;

    assert_author_scoping(&client).await;
    // Before the git-history index has caught up the search is a bounded walk over subjects and
    // authors, which is a cut the caller did not ask for; once it has, the answer is whole. Which of
    // the two this call meets is a race, so each is held to its own contract.
    let first = call(&client, "git_log", json!({ "query": "parser" })).await;
    assert_eq!(summaries(&first), ["fix the parser"], "{first}");
    match first.get("truncated") {
        Some(truncated) => {
            assert_eq!(truncated, true, "{first}");
            assert_eq!(first["truncated_reason"], "live_walk", "{first}");
        }
        None => assert!(
            first.get("truncated_reason").is_none(),
            "an index answer is whole: {first}"
        ),
    }

    // The server is meant to build the git-history index, unless `BASEMIND_GH_INDEX=0` turns it off
    // and every answer is the live walk; an index that never comes up fails the test.
    if std::env::var("BASEMIND_GH_INDEX").is_ok_and(|value| value == "0") {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let probe = call(&client, "git_log", json!({ "query": "parser" })).await;
        assert!(probe.get("partial").is_none(), "{probe}");
        if probe.get("truncated").and_then(Value::as_bool) != Some(true) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the git-history index did not answer the search within 60 s: {probe}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_author_scoping(&client).await;
}

/// Wait until the git-history index, not the bounded live walk, answers a search for `query`, and
/// return that answer.
async fn indexed_search(client: &Client, query: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let probe = call(client, "git_log", json!({ "query": query })).await;
        if probe.get("truncated").and_then(Value::as_bool) != Some(true) {
            return probe;
        }
        assert!(
            Instant::now() < deadline,
            "the git-history index did not answer `{query}` within 60 s: {probe}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A commit made while the server runs reaches the git-history index: the index catches up with the
/// new HEAD instead of leaving every later history call on the live walk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_log_index_catches_up_with_a_commit_made_after_the_server_started() {
    if std::env::var("BASEMIND_GH_INDEX").is_ok_and(|value| value == "0") {
        return;
    }
    let (dir, client) = started().await;
    let root = dir.path();
    indexed_search(&client, "init").await;

    let outline = std::fs::read_to_string(root.join("src/outline.rs")).unwrap();
    std::fs::write(root.join("src/outline.rs"), outline.replace("    1\n", "    13\n")).unwrap();
    commit_as(root, "carol", "carol@e.x", "tune the zeppelin");

    let caught_up = indexed_search(&client, "zeppelin").await;
    assert_eq!(summaries(&caught_up), ["tune the zeppelin"], "{caught_up}");
}

/// A directory named without its trailing slash is still a directory: with the git-history index
/// answering, its history is the commits under it, as with the slash, not an empty page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_log_of_a_directory_without_its_slash_lists_the_commits_under_it() {
    let (_dir, client) = started().await;
    if std::env::var("BASEMIND_GH_INDEX").is_ok_and(|value| value == "0") {
        return;
    }
    indexed_search(&client, "init").await;
    let slashed = call(&client, "git_log", json!({ "path": "src/" })).await;
    let bare = call(&client, "git_log", json!({ "path": "src" })).await;
    assert_eq!(summaries(&slashed), ["init"], "{slashed}");
    assert_eq!(bare["commits"], slashed["commits"], "{bare}");
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
    let changes: Vec<(String, String)> = whole["history"]
        .as_array()
        .expect("history")
        .iter()
        .map(history_change)
        .collect();
    assert_eq!(changes, [("introduced".to_string(), "init".to_string())], "{whole}");
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
    assert_eq!(
        history_change(&churn["history"][0]),
        ("modified".to_string(), "round 15".to_string()),
        "{churn}"
    );
}

/// Every page of a symbol's history, following `next_cursor` until there is none.
async fn whole_symbol_history(client: &Client, arguments: Value) -> (Vec<(String, String)>, usize) {
    let mut changes = Vec::new();
    let mut arguments = arguments;
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages < 50, "the cursor must make progress");
        let page = call(client, "git_log", arguments.clone()).await;
        for entry in page["history"].as_array().expect("history") {
            changes.push(history_change(entry));
        }
        match page.get("next_cursor") {
            Some(cursor) => {
                assert!(
                    !page["history"].as_array().unwrap().is_empty() || page["truncated_reason"] == "window",
                    "an empty page that goes on says why: {page}"
                );
                arguments["cursor"] = cursor.clone();
            }
            None => {
                assert!(
                    page.get("truncated").is_none(),
                    "the last page is the whole history: {page}"
                );
                return (changes, pages);
            }
        }
    }
}

/// A window that ends before the history does hands back a cursor, so a change older than the
/// first window, the introduction included, is a matter of following it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_log_of_a_symbol_pages_past_its_window_to_the_introduction() {
    let (dir, client) = started_with(&[("src/steady.rs", STEADY_RS)]).await;
    let root = dir.path();
    for round in 1..=30 {
        std::fs::write(
            root.join("src/steady.rs"),
            STEADY_RS.replace("    0\n", &format!("    {round}\n")),
        )
        .unwrap();
        super::git(root, &["commit", "-qam", &format!("round {round}")]);
    }

    let first = call(
        &client,
        "git_log",
        json!({ "path": "src/steady.rs", "symbol": "stable", "limit": 1 }),
    )
    .await;
    assert_eq!(first["history"], json!([]), "{first}");
    assert_eq!(first["commits_inspected"], 8, "{first}");
    assert!(first["next_cursor"].is_string(), "31 commits touch the file: {first}");

    let (stable, pages) = whole_symbol_history(
        &client,
        json!({ "path": "src/steady.rs", "symbol": "stable", "limit": 1 }),
    )
    .await;
    assert_eq!(
        stable,
        [("introduced".to_string(), "init".to_string())],
        "after {pages} pages"
    );
    assert_eq!(pages, 4, "31 commits in windows of 8");

    let (churn, _) = whole_symbol_history(
        &client,
        json!({ "path": "src/steady.rs", "symbol": "churn", "limit": 3 }),
    )
    .await;
    let mut expected: Vec<(String, String)> = (1..=30)
        .rev()
        .map(|round| ("modified".to_string(), format!("round {round}")))
        .collect();
    expected.push(("introduced".to_string(), "init".to_string()));
    assert_eq!(churn, expected, "every change once, newest first, across the pages");
}

/// A function removed from the working tree still has a history. Only a name that no commit
/// defines either is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_log_of_a_symbol_the_working_tree_no_longer_has_reads_its_history() {
    let (dir, client) = started_with(&[("src/steady.rs", STEADY_RS)]).await;
    let root = dir.path();
    std::fs::write(root.join("src/steady.rs"), "pub fn stable() -> u32 {\n    7\n}\n").unwrap();
    super::git(root, &["commit", "-qam", "drop churn"]);
    call(&client, "index_rescan", json!({ "paths": ["src/steady.rs"] })).await;
    let outline = call(&client, "code_outline", json!({ "path": "src/steady.rs" })).await;
    assert_eq!(outline["total"], 1, "the index no longer has `churn`: {outline}");

    let (churn, _) = whole_symbol_history(&client, json!({ "path": "src/steady.rs", "symbol": "churn" })).await;
    assert_eq!(
        churn,
        [
            ("removed".to_string(), "drop churn".to_string()),
            ("introduced".to_string(), "init".to_string())
        ]
    );

    let never = call_err(
        &client,
        "git_log",
        json!({ "path": "src/steady.rs", "symbol": "never_defined" }),
    )
    .await;
    assert!(
        never.contains("no symbol `never_defined` in `src/steady.rs`"),
        "{never}"
    );
}

/// A mistyped symbol on a file with more commits than one window never pages silently: each empty
/// page says the name is defined nowhere so far, and the last page is the error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_log_of_a_mistyped_symbol_says_so_on_every_page_and_ends_in_the_error() {
    let (dir, client) = started_with(&[("src/steady.rs", STEADY_RS)]).await;
    let root = dir.path();
    for round in 1..=20 {
        std::fs::write(
            root.join("src/steady.rs"),
            STEADY_RS.replace("    0\n", &format!("    {round}\n")),
        )
        .unwrap();
        super::git(root, &["commit", "-qam", &format!("round {round}")]);
    }

    let mut arguments = json!({ "path": "src/steady.rs", "symbol": "stabel", "limit": 1 });
    let mut pages = 0;
    let refusal = loop {
        pages += 1;
        assert!(pages < 10, "the cursor must make progress");
        let result = super::call_raw(&client, "git_log", arguments.clone()).await;
        let text = super::text_of(&result);
        if result.is_error == Some(true) {
            break text;
        }
        let page: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(page["history"], json!([]), "{page}");
        let why = page["unmatched"].as_str().unwrap_or_default();
        assert!(
            why.contains("no symbol `stabel` in `src/steady.rs`") && why.contains("stable"),
            "an empty page names the mistake and the candidates: {page}"
        );
        arguments["cursor"] = page["next_cursor"].clone();
    };
    assert_eq!(pages, 3, "21 commits in windows of 8, the last page being the error");
    assert!(
        refusal.contains("no symbol `stabel` in `src/steady.rs`") && refusal.contains("stable"),
        "{refusal}"
    );
}

/// A symbol removed longer ago than one window is not a mistake: the pages before its removal say
/// nothing has defined it yet, and the history is still reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_log_of_a_symbol_removed_before_the_first_window_still_reaches_its_history() {
    let (dir, client) = started_with(&[("src/steady.rs", STEADY_RS)]).await;
    let root = dir.path();
    let without_churn = "pub fn stable() -> u32 {\n    7\n}\n";
    for round in 1..=20 {
        let body = if round < 3 {
            STEADY_RS.replace("    0\n", &format!("    {round}\n"))
        } else {
            format!("{without_churn}// round {round}\n")
        };
        std::fs::write(root.join("src/steady.rs"), body).unwrap();
        super::git(root, &["commit", "-qam", &format!("round {round}")]);
    }
    call(&client, "index_rescan", json!({ "paths": ["src/steady.rs"] })).await;

    let first = call(
        &client,
        "git_log",
        json!({ "path": "src/steady.rs", "symbol": "churn", "limit": 1 }),
    )
    .await;
    assert!(
        first["unmatched"]
            .as_str()
            .is_some_and(|why| why.contains("no symbol `churn`")),
        "{first}"
    );
    let (churn, pages) = whole_symbol_history(
        &client,
        json!({ "path": "src/steady.rs", "symbol": "churn", "limit": 1 }),
    )
    .await;
    assert_eq!(
        churn,
        [
            ("removed".to_string(), "round 3".to_string()),
            ("modified".to_string(), "round 2".to_string()),
            ("modified".to_string(), "round 1".to_string()),
            ("introduced".to_string(), "init".to_string())
        ],
        "after {pages} pages"
    );
}

/// `git_blame {symbol}` blames the lines the symbol has in the blamed revision. The working-tree
/// outline names a different set of lines as soon as the file has changed above the symbol.
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
    assert_eq!(
        blame_hunks(&at_head),
        [("7-9".to_string(), "init".to_string())],
        "{at_head}"
    );

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
    assert_eq!(blame_hunks(&span)[0].1, "init", "{span}");
}
