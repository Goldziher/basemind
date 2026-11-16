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
    assert_eq!(hunks.len(), 200, "the first 200 hunks are returned");
    assert_eq!(diff["truncated"], true, "{}", diff["omitted"]);
    assert_eq!(diff["truncated_reason"], "hunk_cap");
    assert_eq!(diff["omitted"], 800, "one hunk per changed line, 1000 in all");
    assert!(diff.get("total_hunks").is_none(), "one name for what was left out");

    let small = call(&client, "git_diff", json!({ "path": "src/outline.rs" })).await;
    assert!(small.get("truncated").is_none(), "{small}");
    assert!(
        small.get("truncated_reason").is_none() && small.get("omitted").is_none(),
        "{small}"
    );
}

/// The byte cap bounds a single hunk too: a new file is one hunk holding the whole file, however
/// large it is.
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
    assert_eq!(diff["truncated_reason"], "byte_cap");
    assert!(
        diff.get("omitted").is_none(),
        "the one hunk is there, clipped: {}",
        diff["omitted"]
    );
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

/// A directory history inspects a bounded window of recent commits per page. A window that ends
/// before the history does hands back a cursor to the commit after it, so a commit older than the
/// first window is reached by following it, and the last page is the whole history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_log_of_a_directory_pages_past_its_window() {
    let (dir, client) = started_with(&[("lib/old.rs", "pub fn old() {}\n")]).await;
    let root = dir.path();

    let recent = call(&client, "git_log", json!({ "path": "src/" })).await;
    assert!(
        recent.get("truncated").is_none() && recent.get("next_cursor").is_none(),
        "one commit fits the window: {recent}"
    );

    for round in 0..199 {
        std::fs::write(root.join("src/churn.rs"), format!("pub const ROUND: u32 = {round};\n")).unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", &format!("churn {round}")]);
    }
    let exact = call(&client, "git_log", json!({ "path": "lib/" })).await;
    assert_eq!(exact["commits"][0]["summary"], "init", "{exact}");
    assert!(
        exact.get("truncated").is_none() && exact.get("next_cursor").is_none(),
        "200 commits fill the window exactly and nothing lies beyond it: {exact}"
    );

    std::fs::write(root.join("src/churn.rs"), "pub const ROUND: u32 = 1000;\n").unwrap();
    git(root, &["commit", "-qam", "one past the window"]);

    let filled = call(&client, "git_log", json!({ "path": "src/", "limit": 5 })).await;
    assert_eq!(filled["commits"].as_array().map(Vec::len), Some(5), "{filled}");
    assert!(filled["next_cursor"].is_string(), "{filled}");
    assert!(
        filled.get("truncated").is_none(),
        "a full page is an ordinary page, whatever lies past the window: {filled}"
    );

    let windowed = call(&client, "git_log", json!({ "path": "lib/" })).await;
    assert_eq!(windowed["commits"], json!([]), "{windowed}");
    assert_eq!(windowed["truncated"], true, "{windowed}");
    assert_eq!(windowed["truncated_reason"], "window", "{windowed}");
    assert_eq!(windowed["window_inspected"], 200, "{windowed}");
    let older = call(
        &client,
        "git_log",
        json!({ "path": "lib/", "cursor": windowed["next_cursor"] }),
    )
    .await;
    assert_eq!(older["commits"][0]["summary"], "init", "{older}");
    assert_eq!(older["window_inspected"], 1, "{older}");
    assert!(
        older.get("truncated").is_none() && older.get("next_cursor").is_none(),
        "the last window is the end of the history: {older}"
    );

    // A mistyped directory never pages silently: the empty page says why, the last one is the error.
    let mistyped = call(&client, "git_log", json!({ "path": "scr/" })).await;
    assert_eq!(mistyped["commits"], json!([]), "{mistyped}");
    assert!(
        mistyped["unmatched"]
            .as_str()
            .is_some_and(|why| why.contains("no such directory")),
        "{mistyped}"
    );
    let refused = call_err(
        &client,
        "git_log",
        json!({ "path": "scr/", "cursor": mistyped["next_cursor"] }),
    )
    .await;
    assert!(
        refused.contains("no commit reachable from HEAD touches `scr/`"),
        "{refused}"
    );

    // Every commit touching the directory, once, across pages cut by `limit` and by the window.
    let mut summaries: Vec<String> = Vec::new();
    let mut arguments = json!({ "path": "src/", "limit": 100 });
    for _ in 0..10 {
        let page = call(&client, "git_log", arguments.clone()).await;
        for commit in page["commits"].as_array().expect("commits") {
            summaries.push(commit["summary"].as_str().unwrap().to_string());
        }
        match page.get("next_cursor") {
            Some(cursor) => arguments["cursor"] = cursor.clone(),
            None => break,
        }
    }
    assert_eq!(summaries.len(), 201, "{summaries:?}");
    assert_eq!(summaries.first().map(String::as_str), Some("one past the window"));
    assert_eq!(summaries.last().map(String::as_str), Some("init"));
    summaries.dedup();
    assert_eq!(summaries.len(), 201, "no commit twice");
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
    assert_eq!(shown["budgeted"], true, "a token-budget cut, like every other list's");
    assert_eq!(
        shown["omitted"].as_u64(),
        Some(600 - files as u64),
        "the rest are counted"
    );
    assert!(shown.get("files_omitted").is_none() && shown.get("truncated").is_none());
    assert_eq!(shown["commit"]["summary"], "six hundred files");

    let whole = call(&client, "git_log", json!({ "rev": "HEAD", "max_tokens": 100000 })).await;
    assert_eq!(whole["files"].as_array().expect("files").len(), 600);
    assert!(whole.get("omitted").is_none() && whole.get("budgeted").is_none());
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
    assert_eq!(status["truncated"], true, "{}", status["omitted"]);
    assert_eq!(status["truncated_reason"], "path_cap");
    assert_eq!(status["is_clean"], false);

    // A cut list names the way to see past it: the status under one directory.
    for name in ["a.txt", "b.txt"] {
        std::fs::write(root.join("src").join(name), "x\n").unwrap();
    }
    let narrowed = call(&client, "git_diff", json!({ "path": "src/" })).await;
    assert_eq!(narrowed["untracked"], json!(["src/a.txt", "src/b.txt"]), "{narrowed}");
    assert_eq!(narrowed["path"], "src/");
    assert!(
        narrowed.get("truncated").is_none() && narrowed.get("omitted").is_none(),
        "{narrowed}"
    );
    let refused = call_err(&client, "git_diff", json!({ "path": "src/", "from": "HEAD" })).await;
    assert!(
        refused.contains("a directory `path` narrows the working-tree status"),
        "{refused}"
    );
    for name in ["a.txt", "b.txt"] {
        std::fs::remove_file(root.join("src").join(name)).unwrap();
    }
    let unchanged = call(&client, "git_diff", json!({ "path": "src/" })).await;
    assert_eq!(unchanged["is_clean"], true, "nothing changed under it: {unchanged}");

    for index in 0..600 {
        std::fs::remove_file(root.join(format!("untracked_{index:03}.txt"))).unwrap();
    }
    let clean = call(&client, "git_diff", json!({})).await;
    assert!(
        clean.get("omitted").is_none() && clean.get("truncated").is_none(),
        "{clean}"
    );
    assert_eq!(clean["is_clean"], true, "{clean}");
}

/// A directory nothing tracks is listed file by file when the status is narrowed to it or to a
/// directory inside it, whatever `status.showUntrackedFiles` collapses in the whole-tree status.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_status_under_an_untracked_directory_lists_its_files() {
    let (dir, client) = started().await;
    let root = dir.path();
    git(root, &["config", "status.showUntrackedFiles", "normal"]);
    std::fs::create_dir_all(root.join("new/sub")).unwrap();
    for name in ["a.txt", "b.txt"] {
        std::fs::write(root.join("new/sub").join(name), "x\n").unwrap();
    }
    std::fs::write(root.join("new/top.txt"), "x\n").unwrap();

    let whole = call(&client, "git_diff", json!({})).await;
    assert_eq!(whole["is_clean"], false, "{whole}");

    let nested = call(&client, "git_diff", json!({ "path": "new/sub/" })).await;
    assert_eq!(
        nested["untracked"],
        json!(["new/sub/a.txt", "new/sub/b.txt"]),
        "{nested}"
    );
    assert_eq!(nested["is_clean"], false, "{nested}");

    let outer = call(&client, "git_diff", json!({ "path": "new/" })).await;
    assert_eq!(
        outer["untracked"],
        json!(["new/sub/a.txt", "new/sub/b.txt", "new/top.txt"]),
        "{outer}"
    );
}

/// A directory that is not on disk and has no changed path under it is a mistake in the call, not
/// a clean directory; so is a directory given where a file's diff is asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_diff_refuses_a_directory_that_does_not_exist_or_lacks_its_slash() {
    let (dir, client) = started_with(&[("tests/it.rs", "pub fn it() {}\n")]).await;
    let root = dir.path();

    let mistyped = call_err(&client, "git_diff", json!({ "path": "scr/" })).await;
    assert!(mistyped.contains("no directory `scr/`"), "{mistyped}");
    let a_file = call_err(&client, "git_diff", json!({ "path": "tools.py/" })).await;
    assert!(a_file.contains("no directory `tools.py/`"), "{a_file}");

    for path in ["src", "tests"] {
        let bare = call_err(&client, "git_diff", json!({ "path": path })).await;
        assert!(bare.contains(&format!("`{path}/`")), "{bare}");
        let structural = call_err(&client, "git_diff", json!({ "path": path, "structural": true })).await;
        assert!(structural.contains(&format!("`{path}/`")), "{structural}");
    }

    // A directory deleted from the working tree is gone from disk and still has a status.
    std::fs::remove_dir_all(root.join("tests")).unwrap();
    let deleted = call(&client, "git_diff", json!({ "path": "tests/" })).await;
    assert_eq!(deleted["modified"], json!(["tests/it.rs"]), "{deleted}");
    let bare = call_err(&client, "git_diff", json!({ "path": "tests" })).await;
    assert!(bare.contains("`tests/`"), "a tree at HEAD: {bare}");
}

/// A read over the size cap ends at the end of a line and reports the line it really ends on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_read_cuts_an_oversized_read_at_a_line_and_reports_where() {
    let mut big = String::from("pub fn enormous() {\n");
    for index in 0..4_000 {
        let _ = writeln!(big, "    let caf\u{e9}_{index:04} = \"{}\";", "\u{e9}".repeat(20));
    }
    big.push_str("}\n");
    let (_dir, client) = started_with(&[("src/big.rs", big.as_str()), ("src/tiny.rs", "pub fn tiny() {}\n")]).await;
    let lines: Vec<&str> = big.lines().collect();

    for arguments in [
        json!({ "path": "src/big.rs", "line": 1, "end_line": 4002 }),
        json!({ "path": "src/big.rs", "symbol": "enormous" }),
    ] {
        let read = call(&client, "code_read", arguments).await;
        assert_eq!(read["truncated"], true, "{}", read["bytes"]);
        assert_eq!(read["truncated_reason"], "size_cap");
        assert_eq!(
            read["bytes"].as_u64(),
            Some(big.len() as u64 - 1),
            "the whole span's size"
        );
        let body = read["body"].as_str().expect("body");
        assert!(body.len() <= 128 * 1024, "{} bytes", body.len());
        assert!(!body.contains('\u{fffd}'), "no character was cut in half");
        let returned: Vec<&str> = body.lines().collect();
        assert!(returned.len() > 1_000, "{} lines", returned.len());
        assert_eq!(returned, lines[..returned.len()], "whole lines only");
        assert_eq!(read["line"], 1);
        assert_eq!(
            read["end_line"].as_u64(),
            Some(returned.len() as u64),
            "the last line returned, not the last line asked for"
        );
        assert_eq!(
            read["omitted_bytes"].as_u64(),
            Some((big.len() - 1 - body.len()) as u64),
            "the bytes left out"
        );
        assert!(
            read.get("omitted").is_none(),
            "`omitted` counts items: {}",
            read["bytes"]
        );

        // The cut names where to go on: the line after `end_line`.
        let rest = call(
            &client,
            "code_read",
            json!({ "path": "src/big.rs", "line": returned.len() + 1, "end_line": 4002 }),
        )
        .await;
        let resumed: Vec<&str> = rest["body"].as_str().expect("body").lines().collect();
        assert_eq!(resumed[0], lines[returned.len()], "no line lost or repeated at the cut");
    }

    let small = call(
        &client,
        "code_read",
        json!({ "path": "src/big.rs", "line": 4001, "end_line": 9999 }),
    )
    .await;
    assert_eq!(small["body"], format!("{}\n}}", lines[4000]), "{small}");
    assert_eq!(small["end_line"], 4002, "{small}");
    assert_eq!(small["total_lines"], 4002, "{small}");
    // A read that was not cut leaves `truncated` out, as every other tool does.
    let tiny = call(&client, "code_read", json!({ "path": "src/tiny.rs", "symbol": "tiny" })).await;
    assert_eq!(tiny["body"], "pub fn tiny() {}", "{tiny}");
    for whole in [&small, &tiny] {
        assert!(
            whole.get("truncated").is_none()
                && whole.get("truncated_reason").is_none()
                && whole.get("omitted_bytes").is_none(),
            "{whole}"
        );
    }
}

const TWO_NEW_RS: &str = "\
pub struct A;
pub struct B;

impl A {
    pub fn new() -> Self {
        A
    }
}

impl B {
    pub fn new() -> Self {
        B
    }
}
";

/// `added` and `removed` name a symbol by name and kind, so `common` counts by the same identity:
/// two `new` methods are one entry, not two.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_diff_structural_counts_a_repeated_symbol_once() {
    let (_dir, client) = started_with(&[("src/two_new.rs", TWO_NEW_RS)]).await;

    let outline = call(&client, "code_outline", json!({ "path": "src/two_new.rs" })).await;
    let symbols = outline["symbols"].as_array().expect("symbols");
    let distinct: std::collections::BTreeSet<(&str, &str)> = symbols
        .iter()
        .map(|symbol| (symbol["name"].as_str().unwrap(), symbol["kind"].as_str().unwrap()))
        .collect();
    assert!(
        distinct.len() < symbols.len(),
        "the fixture repeats a symbol: {outline}"
    );

    let diff = call(
        &client,
        "git_diff",
        json!({ "path": "src/two_new.rs", "structural": true }),
    )
    .await;
    assert_eq!(diff["common"].as_u64(), Some(distinct.len() as u64), "{diff}");
    assert_eq!((&diff["added"], &diff["removed"]), (&json!([]), &json!([])), "{diff}");
}
