//! Every response is bounded by default, and an out-of-range argument is an instructive error
//! instead of an internal one.

use std::fmt::Write as _;

use serde_json::json;

use super::{call, call_err, git, names, started, started_with};

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

/// The byte cap bounds a single hunk too. A new file is one hunk holding the whole file, which used
/// to be returned in full however large it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_diff_clips_one_oversized_hunk_and_says_so() {
    let (dir, client) = started().await;
    let mut added = String::new();
    for line in 0..12_000 {
        let _ = writeln!(added, "é line {line} of a generated file");
    }
    assert!(added.len() > 300 * 1024, "{} bytes", added.len());
    std::fs::write(dir.path().join("generated.txt"), &added).unwrap();

    let diff = call(&client, "git_diff", json!({ "path": "generated.txt" })).await;
    let hunks = diff["hunks"].as_array().expect("hunks");
    assert_eq!(hunks.len(), 1, "a new file is one hunk");
    let text = hunks[0]["text"].as_str().expect("text");
    assert!(text.len() <= 128 * 1024, "{} bytes of hunk text", text.len());
    assert!(
        text.len() > 100 * 1024,
        "the cap is used, not undershot: {}",
        text.len()
    );
    assert_eq!(diff["truncated"], true, "a clipped hunk is a truncated diff");
    assert_eq!(diff["total_hunks"], 1);
}

/// A file over the per-side ceiling is not diffed at all, and the response says why it has no hunks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_diff_reports_a_file_too_large_to_diff() {
    let (dir, client) = started().await;
    let line = "a line of an enormous generated file\n";
    let huge = line.repeat(5 * 1024 * 1024 / line.len());
    std::fs::write(dir.path().join("huge.txt"), &huge).unwrap();

    let worktree = call(&client, "git_diff", json!({ "path": "huge.txt" })).await;
    assert_eq!(worktree["too_large"], true, "{}", worktree["truncated"]);
    assert_eq!(worktree["hunks"], json!([]));
    assert_eq!(worktree["present_at_to"], true);

    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "huge"]);
    let committed = call(
        &client,
        "git_diff",
        json!({ "path": "huge.txt", "from": "HEAD~1", "to": "HEAD" }),
    )
    .await;
    assert_eq!(committed["too_large"], true, "{}", committed["truncated"]);
    assert_eq!(committed["hunks"], json!([]));

    let small = call(&client, "git_diff", json!({ "path": "src/outline.rs" })).await;
    assert!(small.get("too_large").is_none(), "{small}");
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

    for round in 0..199 {
        std::fs::write(root.join("src/churn.rs"), format!("pub const ROUND: u32 = {round};\n")).unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "churn"]);
    }
    let exact = call(&client, "git_log", json!({ "path": "src/", "limit": 5 })).await;
    assert!(
        exact.get("truncated").is_none(),
        "200 commits fill the window exactly and nothing lies beyond it: {exact}"
    );

    std::fs::write(root.join("src/churn.rs"), "pub const ROUND: u32 = 1000;\n").unwrap();
    git(root, &["commit", "-qam", "one past the window"]);

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

/// `max_tokens` bounds the file list of a single commit, and what it leaves out is counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_log_of_a_rev_holds_its_file_list_to_max_tokens() {
    let (dir, client) = started().await;
    let root = dir.path();
    for index in 0..600 {
        std::fs::write(root.join(format!("added_{index:03}.txt")), "x\n").unwrap();
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "six hundred files"]);

    let shown = call(&client, "git_log", json!({ "rev": "HEAD", "max_tokens": 200 })).await;
    let files = shown["files"].as_array().expect("files").len();
    assert!((1..100).contains(&files), "200 tokens of file rows, not {files}");
    assert_eq!(
        shown["files_omitted"].as_u64(),
        Some(600 - files as u64),
        "the rest are counted"
    );
    assert_eq!(shown["commit"]["summary"], "six hundred files");

    let whole = call(&client, "git_log", json!({ "rev": "HEAD", "max_tokens": 100000 })).await;
    assert_eq!(whole["files"].as_array().expect("files").len(), 600);
    assert!(whole.get("files_omitted").is_none());
}

/// `max_tokens` bounds a symbol's history like every other `git_log` list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_log_of_a_symbol_honours_max_tokens() {
    let (dir, client) = started().await;
    let root = dir.path();
    let source = std::fs::read_to_string(root.join("src/outline.rs")).unwrap();
    for round in 2..=4 {
        std::fs::write(
            root.join("src/outline.rs"),
            source.replace("    1\n", &format!("    {round}\n")),
        )
        .unwrap();
        git(root, &["commit", "-qam", &format!("round {round}")]);
    }

    let arguments = json!({ "path": "src/outline.rs", "symbol": "file_outline" });
    let whole = call(&client, "git_log", arguments.clone()).await;
    assert_eq!(whole["history"].as_array().expect("history").len(), 4, "{whole}");

    let mut bounded = arguments;
    bounded["max_tokens"] = json!(1);
    let first = call(&client, "git_log", bounded.clone()).await;
    assert_eq!(first["history"].as_array().expect("history").len(), 1, "{first}");
    assert_eq!(first["history"][0]["summary"], "round 4", "{first}");
    assert_eq!(first["budgeted"], true, "{first}");
    bounded["cursor"] = first["next_cursor"].clone();
    let second = call(&client, "git_log", bounded).await;
    assert_eq!(second["history"][0]["summary"], "round 3", "{second}");
}

/// A structural diff is asked for what changed. The unchanged symbols are a count, not a list as
/// long as the file's outline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_diff_structural_counts_the_unchanged_symbols() {
    let (dir, client) = started().await;
    let path = dir.path().join("src/outline.rs");
    let source = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{source}\npub fn brand_new() {{}}\n")).unwrap();
    call(&client, "index_rescan", json!({ "paths": ["src/outline.rs"] })).await;

    let outline = call(&client, "code_outline", json!({ "path": "src/outline.rs" })).await;
    let diff = call(
        &client,
        "git_diff",
        json!({ "path": "src/outline.rs", "structural": true }),
    )
    .await;
    assert_eq!(names(&diff["added"]), ["brand_new"], "{diff}");
    assert_eq!(
        diff["common"].as_u64(),
        Some(outline["total"].as_u64().unwrap() - 1),
        "{diff}"
    );
}

/// The working-tree status lists a bounded number of paths per bucket and counts the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_status_lists_are_bounded_with_a_count_of_the_rest() {
    let (dir, client) = started().await;
    let root = dir.path();
    for index in 0..600 {
        std::fs::write(root.join(format!("untracked_{index:03}.txt")), "x\n").unwrap();
    }

    let status = call(&client, "git_diff", json!({})).await;
    let listed = status["untracked"].as_array().expect("untracked").len();
    assert_eq!(listed, 200, "{}", status["omitted"]);
    assert_eq!(status["omitted"], 400, "600 untracked files in all");
    assert_eq!(status["is_clean"], false);

    for index in 0..600 {
        std::fs::remove_file(root.join(format!("untracked_{index:03}.txt"))).unwrap();
    }
    let clean = call(&client, "git_diff", json!({})).await;
    assert!(clean.get("omitted").is_none(), "{clean}");
    assert_eq!(clean["is_clean"], true, "{clean}");
}
