//! Every response is bounded by default, and an out-of-range argument is an instructive error
//! instead of an internal one.

use std::fmt::Write as _;

use serde_json::json;

use super::{call, call_err, git, started, started_with};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_blame_validates_its_span_and_limit() {
    let (_dir, client) = started().await;

    let zero_limit = call_err(&client, "git_blame", json!({ "path": "src/outline.rs", "limit": 0 })).await;
    assert!(zero_limit.contains("`limit` must be at least 1"), "{zero_limit}");

    let zero_line = call_err(&client, "git_blame", json!({ "path": "src/outline.rs", "line": 0 })).await;
    assert!(zero_line.contains("lines are 1-based"), "{zero_line}");

    let inverted = call_err(
        &client,
        "git_blame",
        json!({ "path": "src/outline.rs", "line": 5, "end_line": 3 }),
    )
    .await;
    assert!(inverted.contains("`end_line` must be >= `line`"), "{inverted}");

    let past_the_end = call_err(&client, "git_blame", json!({ "path": "src/outline.rs", "line": 9000 })).await;
    assert!(past_the_end.contains("out of range"), "{past_the_end}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_blame_honours_a_token_budget_and_pages_the_rest() {
    let (dir, client) = started().await;
    let outline = std::fs::read_to_string(dir.path().join("src/outline.rs")).unwrap();
    std::fs::write(
        dir.path().join("src/outline.rs"),
        outline.replace("    2\n", "    22\n"),
    )
    .unwrap();
    git(dir.path(), &["commit", "-qam", "bump"]);

    let whole = call(&client, "git_blame", json!({ "path": "src/outline.rs" })).await;
    let total = whole["hunks"].as_array().map(Vec::len).unwrap_or_default();
    assert!(total >= 3, "the edit splits the blame into several hunks: {whole}");

    let first = call(
        &client,
        "git_blame",
        json!({ "path": "src/outline.rs", "max_tokens": 1 }),
    )
    .await;
    assert_eq!(first["hunks"].as_array().map(Vec::len), Some(1), "{first}");
    assert_eq!(first["budgeted"], true, "{first}");
    let cursor = first["next_cursor"].clone();
    assert!(cursor.is_string(), "{first}");

    let rest = call(
        &client,
        "git_blame",
        json!({ "path": "src/outline.rs", "cursor": cursor }),
    )
    .await;
    assert_eq!(rest["hunks"].as_array().map(Vec::len), Some(total - 1), "{rest}");
    assert_eq!(rest["hunks"][0], whole["hunks"][1], "{rest}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_diff_reports_a_binary_file_as_binary() {
    let (dir, client) = started().await;
    let root = dir.path();
    std::fs::write(root.join("blob.bin"), [0u8, 159, 146, 150, 0, 1, 2, 3]).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "binary"]);
    std::fs::write(root.join("blob.bin"), [0u8, 159, 146, 150, 0, 9, 9, 9, 9]).unwrap();

    let diff = call(&client, "git_diff", json!({ "path": "blob.bin" })).await;
    assert_eq!(diff["binary"], true, "{diff}");
    assert_eq!(diff["hunks"], json!([]), "{diff}");

    let text = call(&client, "git_diff", json!({ "path": "src/outline.rs" })).await;
    assert!(text.get("binary").is_none(), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_diff_caps_a_huge_diff_and_says_so() {
    let mut before = String::new();
    let mut after = String::new();
    for line in 0..2_000 {
        let _ = writeln!(before, "pub const LINE_{line}: u32 = {line};");
        if line % 2 == 0 {
            let _ = writeln!(after, "pub const LINE_{line}: u32 = {line} + 1;");
        } else {
            let _ = writeln!(after, "pub const LINE_{line}: u32 = {line};");
        }
    }
    let (dir, client) = started_with(&[("src/wide_diff.rs", before.as_str())]).await;
    std::fs::write(dir.path().join("src/wide_diff.rs"), after).unwrap();

    let diff = call(&client, "git_diff", json!({ "path": "src/wide_diff.rs" })).await;
    let hunks = diff["hunks"].as_array().expect("hunks");
    assert_eq!(diff["truncated"], true, "{}", diff["total_hunks"]);
    assert_eq!(diff["total_hunks"], 1_000, "one hunk per changed line");
    assert_eq!(hunks.len(), 200, "the first 200 hunks are returned");

    let small = call(&client, "git_diff", json!({ "path": "src/outline.rs" })).await;
    assert!(small.get("truncated").is_none(), "{small}");
}

/// A directory history inspects a bounded window of recent commits. A window that was used up is
/// reported, so "no more commits" is not mistaken for the whole history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_log_of_a_directory_says_when_its_window_was_exhausted() {
    let (dir, client) = started().await;
    let root = dir.path();

    let recent = call(&client, "git_log", json!({ "path": "src/" })).await;
    assert!(
        recent.get("truncated").is_none(),
        "one commit fits the window: {recent}"
    );

    for round in 0..205 {
        std::fs::write(root.join("src/churn.rs"), format!("pub const ROUND: u32 = {round};\n")).unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "churn"]);
    }

    let windowed = call(&client, "git_log", json!({ "path": "src/", "limit": 5 })).await;
    assert_eq!(windowed["truncated"], true, "{windowed}");
    assert_eq!(windowed["truncated_reason"], "window", "{windowed}");
    assert_eq!(windowed["window_inspected"], 200, "{windowed}");
}

/// A hit on a long line carries the text around the match, not the first bytes of the line, and the
/// context lines are clipped the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_clips_long_lines_around_the_match() {
    let filler = "x".repeat(1_500);
    let wide = format!("// {filler}\n// {filler} WIDE_NEEDLE {filler}\n// {filler}\n");
    let (_dir, client) = started_with(&[("src/wide.rs", wide.as_str())]).await;

    let body = call(
        &client,
        "code_grep",
        json!({ "pattern": "WIDE_NEEDLE", "context": true }),
    )
    .await;
    let hit = &body["hits"][0];
    let text = hit["text"].as_str().expect("text");
    assert!(text.contains("WIDE_NEEDLE"), "the window holds the match: {text}");
    assert!(text.len() <= 420, "{} bytes", text.len());
    assert!(text.starts_with('…') && text.ends_with('…'), "{text}");
    assert_eq!(
        hit["column"], 1_505,
        "the column still counts from the start of the line"
    );
    for key in ["context_before", "context_after"] {
        let context = hit[key].as_str().expect("context line");
        assert!(context.len() <= 420, "{key}: {} bytes", context.len());
    }
}

/// The enclosing symbol comes from the indexed outline. When the file changed size since it was
/// indexed its byte spans no longer line up, so the annotation is left out instead of being wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_grep_names_no_symbol_for_a_file_edited_since_the_scan() {
    let (dir, client) = started().await;
    let path = dir.path().join("src/outline.rs");

    let fresh = call(&client, "code_grep", json!({ "pattern": "file_outline() +" })).await;
    assert_eq!(fresh["hits"][0]["symbol"]["name"], "render", "{fresh}");

    let source = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("// a new first line\n// and another\n{source}")).unwrap();

    let edited = call(&client, "code_grep", json!({ "pattern": "file_outline() +" })).await;
    assert_eq!(edited["hits"][0]["line"], 21, "{edited}");
    assert!(edited["hits"][0].get("symbol").is_none(), "{edited}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_read_lists_a_bounded_set_of_candidates_with_their_lines() {
    let mut many = String::new();
    for index in 0..80 {
        let _ = writeln!(many, "pub fn generated_{index}() {{}}");
    }
    let (_dir, client) = started_with(&[("src/many.rs", many.as_str())]).await;

    let small = call_err(
        &client,
        "code_read",
        json!({ "path": "src/outline.rs", "symbol": "nope" }),
    )
    .await;
    assert!(small.contains("no symbol `nope` in `src/outline.rs`"), "{small}");
    assert!(small.contains("file_outline (lines 3-5)"), "{small}");

    let large = call_err(&client, "code_read", json!({ "path": "src/many.rs", "symbol": "nope" })).await;
    assert!(large.contains("generated_0 (line 1)"), "{large}");
    assert!(!large.contains("generated_40"), "the list is bounded: {large}");
    assert!(large.contains("and 60 more"), "{large}");
}
