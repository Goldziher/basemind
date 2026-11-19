//! `git_log`, `git_blame` and `git_diff` answer byte for byte, and `basemind git churn` / `git by-path`
//! print the same JSON: the responses over a repository whose commits carry fixed dates (so their
//! shas never change), and every refusal's exact words.
//!
//! The tool bodies move from the MCP adapter into the engine; these pin what an agent reads so the
//! move (and any later one) cannot change a response or an error text unnoticed.

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::{Client, call_err, call_raw, serve_synced, text_of};

const LIB_V1: &str = "pub fn alpha() -> u32 {\n    1\n}\n\npub fn beta() -> u32 {\n    2\n}\n";
const LIB_V2: &str = "pub fn alpha() -> u32 {\n    10\n}\n\npub fn beta() -> u32 {\n    2\n}\n";
const GAMMA: &str = "pub fn gamma() {}\n";

/// `git` with a fixed author, committer and date, so a commit's sha is the same on every run.
fn git_at(repo: &Path, date: &str, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

fn commit(repo: &Path, date: &str, message: &str) {
    git_at(repo, date, &["add", "-A"]);
    git_at(repo, date, &["commit", "-qm", message]);
}

/// Three dated commits: `init` (`src/lib.rs`, `notes/readme.txt`), `tune alpha`, `add gamma`.
fn dated_repo() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git_at(root, "2024-01-01T00:00:00Z", &["init", "-q", "-b", "main"]);
    git_at(root, "2024-01-01T00:00:00Z", &["config", "commit.gpgsign", "false"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("notes")).unwrap();
    std::fs::write(root.join("src/lib.rs"), LIB_V1).unwrap();
    std::fs::write(root.join("notes/readme.txt"), "hello\n").unwrap();
    commit(root, "2024-01-01T00:00:00Z", "init");
    std::fs::write(root.join("src/lib.rs"), LIB_V2).unwrap();
    commit(root, "2024-01-02T00:00:00Z", "tune alpha");
    std::fs::write(root.join("src/gamma.rs"), GAMMA).unwrap();
    commit(root, "2024-01-03T00:00:00Z", "add gamma");
    dir
}

async fn dated() -> (TempDir, Client) {
    serve_synced(dated_repo()).await
}

/// `text` with every measured `elapsed_us` zeroed, the one field that varies per call.
fn zero_elapsed(text: &str) -> String {
    let key = "\"elapsed_us\":";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(key) {
        let (head, tail) = rest.split_at(at + key.len());
        out.push_str(head);
        let digits = tail.find(|c: char| !c.is_ascii_digit()).unwrap_or(tail.len());
        out.push('0');
        rest = &tail[digits..];
    }
    out.push_str(rest);
    out
}

/// The tool's JSON text, `elapsed_us` zeroed.
async fn answer(client: &Client, tool: &'static str, arguments: Value) -> String {
    let result = call_raw(client, tool, arguments.clone()).await;
    let text = text_of(&result);
    assert_ne!(result.is_error, Some(true), "{tool} {arguments} failed: {text}");
    zero_elapsed(&text)
}

/// Compare every `(label, actual, expected)` at once, so one run shows each drifted answer.
fn assert_all(rows: &[(String, String, String)]) {
    let drifted: Vec<String> = rows
        .iter()
        .filter(|(_, actual, expected)| actual != expected)
        .map(|(label, actual, expected)| format!("{label}\n  actual:   {actual}\n  expected: {expected}"))
        .collect();
    assert!(
        drifted.is_empty(),
        "{} answers drifted:\n{}",
        drifted.len(),
        drifted.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_log_answers_byte_for_byte() {
    let (_dir, client) = dated().await;
    let cases = [
        (
            json!({}),
            r#"{"commits":[{"sha":"c48e32cbf7dd","summary":"add gamma","author":"t","author_time_unix":1704240000,"files":{"src/gamma.rs":"added"}},{"sha":"0c2fe19c38a2","summary":"tune alpha","author":"t","author_time_unix":1704153600,"files":{"src/lib.rs":"modified"}},{"sha":"3f38556b6d45","summary":"init","author":"t","author_time_unix":1704067200,"files":{"notes/readme.txt":"added","src/lib.rs":"added"}}]}"#,
        ),
        (
            json!({ "path": "src/lib.rs" }),
            r#"{"commits":[{"sha":"0c2fe19c38a2","summary":"tune alpha","author":"t","author_time_unix":1704153600},{"sha":"3f38556b6d45","summary":"init","author":"t","author_time_unix":1704067200}]}"#,
        ),
        (
            json!({ "query": "alpha" }),
            r#"{"commits":[{"sha":"0c2fe19c38a2","summary":"tune alpha","author":"t","author_time_unix":1704153600}]}"#,
        ),
        (
            json!({ "author": "t", "limit": 1 }),
            r#"{"commits":[{"sha":"c48e32cbf7dd","summary":"add gamma","author":"t","author_time_unix":1704240000}],"next_cursor":"03020ab9.kgHOxI4yyw"}"#,
        ),
        (
            json!({ "rev": "HEAD~1" }),
            r#"{"commit":{"sha":"0c2fe19c38a28e34c3b8c415828fb9f16d695dad","summary":"tune alpha","author":"t","author_email":"t@e.x","author_time_unix":1704153600},"files":{"src/lib.rs":"modified"}}"#,
        ),
        (
            json!({ "path": "src/" }),
            r#"{"window_inspected":3,"commits":[{"sha":"c48e32cbf7dd","summary":"add gamma","author":"t","author_time_unix":1704240000,"files":{"src/gamma.rs":"added"}},{"sha":"0c2fe19c38a2","summary":"tune alpha","author":"t","author_time_unix":1704153600,"files":{"src/lib.rs":"modified"}},{"sha":"3f38556b6d45","summary":"init","author":"t","author_time_unix":1704067200,"files":{"notes/readme.txt":"added","src/lib.rs":"added"}}]}"#,
        ),
        (
            json!({ "path": "src" }),
            r#"{"window_inspected":3,"commits":[{"sha":"c48e32cbf7dd","summary":"add gamma","author":"t","author_time_unix":1704240000,"files":{"src/gamma.rs":"added"}},{"sha":"0c2fe19c38a2","summary":"tune alpha","author":"t","author_time_unix":1704153600,"files":{"src/lib.rs":"modified"}},{"sha":"3f38556b6d45","summary":"init","author":"t","author_time_unix":1704067200,"files":{"notes/readme.txt":"added","src/lib.rs":"added"}}]}"#,
        ),
        (
            json!({ "path": "src/lib.rs", "symbol": "alpha" }),
            r#"{"kind":null,"commits_inspected":2,"history":[{"sha":"0c2fe19c38a2","summary":"tune alpha","author":"t","author_time_unix":1704153600,"change":"modified"},{"sha":"3f38556b6d45","summary":"init","author":"t","author_time_unix":1704067200,"change":"introduced"}],"hash_mode":"normalized"}"#,
        ),
        (
            json!({ "limit": 1 }),
            r#"{"commits":[{"sha":"c48e32cbf7dd","summary":"add gamma","author":"t","author_time_unix":1704240000,"files":{"src/gamma.rs":"added"}}],"next_cursor":"fc82f23e.kgHOxI4yyg"}"#,
        ),
    ];
    let mut rows = Vec::new();
    for (arguments, expected) in cases {
        let actual = answer(&client, "git_log", arguments.clone()).await;
        rows.push((format!("git_log {arguments}"), actual, expected.to_string()));
    }
    assert_all(&rows);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_blame_answers_byte_for_byte() {
    let (_dir, client) = dated().await;
    let cases = [
        (
            json!({ "path": "src/lib.rs" }),
            r#"{"hunks":["1 3f38556b6d45","2 0c2fe19c38a2","3-7 3f38556b6d45"],"commits":{"3f38556b6d45":{"author":"t","time":1704067200,"summary":"init"},"0c2fe19c38a2":{"author":"t","time":1704153600,"summary":"tune alpha"}},"elapsed_us":0}"#,
        ),
        (
            json!({ "path": "src/lib.rs", "line": 2, "end_line": 6 }),
            r#"{"hunks":["2 0c2fe19c38a2","3-6 3f38556b6d45"],"commits":{"0c2fe19c38a2":{"author":"t","time":1704153600,"summary":"tune alpha"},"3f38556b6d45":{"author":"t","time":1704067200,"summary":"init"}},"elapsed_us":0}"#,
        ),
        (
            json!({ "path": "src/lib.rs", "symbol": "alpha" }),
            r#"{"line":1,"end_line":3,"hunks":["1 3f38556b6d45","2 0c2fe19c38a2","3 3f38556b6d45"],"commits":{"3f38556b6d45":{"author":"t","time":1704067200,"summary":"init"},"0c2fe19c38a2":{"author":"t","time":1704153600,"summary":"tune alpha"}},"elapsed_us":0}"#,
        ),
        (
            json!({ "path": "src/lib.rs", "symbol": "beta", "rev": "HEAD~2" }),
            r#"{"line":5,"end_line":7,"hunks":["5-7 3f38556b6d45"],"commits":{"3f38556b6d45":{"author":"t","time":1704067200,"summary":"init"}},"elapsed_us":0}"#,
        ),
        (
            json!({ "path": "src/lib.rs", "limit": 1 }),
            r#"{"hunks":["1 3f38556b6d45"],"commits":{"3f38556b6d45":{"author":"t","time":1704067200,"summary":"init"}},"next_cursor":"55dbd757.kgHOxI4yyw","elapsed_us":0}"#,
        ),
    ];
    let mut rows = Vec::new();
    for (arguments, expected) in cases {
        let actual = answer(&client, "git_blame", arguments.clone()).await;
        rows.push((format!("git_blame {arguments}"), actual, expected.to_string()));
    }
    assert_all(&rows);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_diff_answers_byte_for_byte() {
    let (dir, client) = dated().await;
    let mut rows = Vec::new();
    let clean = answer(&client, "git_diff", json!({})).await;
    rows.push((
        "git_diff {} (clean)".to_string(),
        clean,
        r#"{"is_clean":true}"#.to_string(),
    ));

    std::fs::write(dir.path().join("src/lib.rs"), LIB_V2.replace("2\n}", "20\n}")).unwrap();
    std::fs::write(dir.path().join("notes/new.txt"), "new\n").unwrap();
    let cases = [
        (
            json!({}),
            r#"{"modified":["src/lib.rs"],"untracked":["notes/new.txt"],"is_clean":false}"#,
        ),
        (
            json!({ "path": "notes/" }),
            r#"{"untracked":["notes/new.txt"],"path":"notes/","is_clean":false}"#,
        ),
        (
            json!({ "path": "src/lib.rs" }),
            r#"{"path":"src/lib.rs","from":"c48e32cbf7dde9e9c1a4ee2a1decb1560000d2d6","to":"working tree","hunks":[{"old_line_start":6,"old_line_count":1,"new_line_start":6,"new_line_count":1,"text":"-    2\n+    20\n"}]}"#,
        ),
        (
            json!({ "path": "src/lib.rs", "from": "HEAD~2", "to": "HEAD~1" }),
            r#"{"path":"src/lib.rs","from":"3f38556b6d45ba5d791d423ebd10c305c5cc37e3","to":"0c2fe19c38a28e34c3b8c415828fb9f16d695dad","hunks":[{"old_line_start":2,"old_line_count":1,"new_line_start":2,"new_line_count":1,"text":"-    1\n+    10\n"}]}"#,
        ),
        (
            json!({ "path": "src/gamma.rs", "from": "HEAD~1" }),
            r#"{"path":"src/gamma.rs","from":"0c2fe19c38a28e34c3b8c415828fb9f16d695dad","to":"working tree","present_at_from":false,"hunks":[{"old_line_start":1,"old_line_count":0,"new_line_start":1,"new_line_count":1,"text":"pub fn gamma() {}\n"}]}"#,
        ),
        (
            json!({ "path": "src/lib.rs", "structural": true, "from": "HEAD~2" }),
            r#"{"path":"src/lib.rs","rev":"3f38556b6d45ba5d791d423ebd10c305c5cc37e3","added":[],"removed":[],"common":2}"#,
        ),
        (
            json!({ "path": "src/gamma.rs", "structural": true, "from": "HEAD~1" }),
            r#"{"path":"src/gamma.rs","rev":"0c2fe19c38a28e34c3b8c415828fb9f16d695dad","added":[{"name":"gamma","kind":"function"}],"removed":[],"common":0,"note":"path absent at HEAD~1; entire file treated as added"}"#,
        ),
    ];
    for (arguments, expected) in cases {
        let actual = answer(&client, "git_diff", arguments.clone()).await;
        rows.push((format!("git_diff {arguments}"), actual, expected.to_string()));
    }
    assert_all(&rows);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_git_refusal_keeps_its_exact_words() {
    let (_dir, client) = dated().await;
    let cases: [(&'static str, Value, &str); 37] = [
        (
            "git_log",
            json!({ "rev": "HEAD", "path": "src/lib.rs" }),
            r#"`git_log`: `rev` shows one commit and combines with no other filter; accepted shapes: {} (recent commits), {path} (commits touching a file, or a directory when it ends with `/`), {query} and/or {author} (search messages / authors), {path, symbol} (when a symbol's body changed; add `line` when several there share the name), {rev} (one commit in full); `limit`, `cursor` and `max_tokens` page every list"#,
        ),
        (
            "git_log",
            json!({ "rev": "HEAD", "limit": 1 }),
            r#"`git_log`: `rev` shows one commit and takes no `limit` or `cursor`; accepted shapes: {} (recent commits), {path} (commits touching a file, or a directory when it ends with `/`), {query} and/or {author} (search messages / authors), {path, symbol} (when a symbol's body changed; add `line` when several there share the name), {rev} (one commit in full); `limit`, `cursor` and `max_tokens` page every list"#,
        ),
        (
            "git_log",
            json!({ "symbol": "alpha" }),
            r#"`git_log`: `symbol` needs the `path` of the file defining it; accepted shapes: {} (recent commits), {path} (commits touching a file, or a directory when it ends with `/`), {query} and/or {author} (search messages / authors), {path, symbol} (when a symbol's body changed; add `line` when several there share the name), {rev} (one commit in full); `limit`, `cursor` and `max_tokens` page every list"#,
        ),
        (
            "git_log",
            json!({ "symbol": "alpha", "path": "src/lib.rs", "query": "x" }),
            r#"`git_log`: `symbol` history does not combine with `query` or `author`; accepted shapes: {} (recent commits), {path} (commits touching a file, or a directory when it ends with `/`), {query} and/or {author} (search messages / authors), {path, symbol} (when a symbol's body changed; add `line` when several there share the name), {rev} (one commit in full); `limit`, `cursor` and `max_tokens` page every list"#,
        ),
        (
            "git_log",
            json!({ "line": 3 }),
            r#"`git_log`: `line` picks one definition of `symbol` and needs it; accepted shapes: {} (recent commits), {path} (commits touching a file, or a directory when it ends with `/`), {query} and/or {author} (search messages / authors), {path, symbol} (when a symbol's body changed; add `line` when several there share the name), {rev} (one commit in full); `limit`, `cursor` and `max_tokens` page every list"#,
        ),
        (
            "git_log",
            json!({ "query": "x", "path": "src/lib.rs" }),
            r#"`git_log`: `query` / `author` search the whole history and do not combine with `path`; accepted shapes: {} (recent commits), {path} (commits touching a file, or a directory when it ends with `/`), {query} and/or {author} (search messages / authors), {path, symbol} (when a symbol's body changed; add `line` when several there share the name), {rev} (one commit in full); `limit`, `cursor` and `max_tokens` page every list"#,
        ),
        (
            "git_log",
            json!({ "rev": "nope" }),
            r#"`git_log`: cannot resolve `nope`: rev parse failed for nope: couldn't parse revision, input="nope""#,
        ),
        (
            "git_log",
            json!({ "rev": "HEAD^{tree}" }),
            r#"`git_log`: `HEAD^{tree}` is not a commit"#,
        ),
        (
            "git_log",
            json!({ "path": "missing.rs" }),
            r#"`git_log`: no commit reachable from HEAD touches `missing.rs` and no such file exists. `path` is a repo-relative file path, or a directory when it ends with `/`; locate it with `find_files`."#,
        ),
        (
            "git_log",
            json!({ "path": "missing/" }),
            r#"`git_log`: no commit reachable from HEAD touches `missing/` and no such directory exists. `path` is a repo-relative file path, or a directory when it ends with `/`; locate it with `find_files`."#,
        ),
        (
            "git_log",
            json!({ "path": "src/lib.rs", "symbol": "nope" }),
            r#"`git_log`: no symbol `nope` in `src/lib.rs`; it defines: alpha (lines 1-3), beta (lines 5-7)"#,
        ),
        (
            "git_log",
            json!({ "path": "notes/readme.txt", "symbol": "x" }),
            r#"`git_log`: no symbol `x` in `notes/readme.txt`; the file has no such symbols"#,
        ),
        (
            "git_log",
            json!({ "limit": 0 }),
            r#"`git_log`: `limit` must be at least 1; omit it for the default page size."#,
        ),
        (
            "git_log",
            json!({ "cursor": "not-bound" }),
            r#"`git_log`: this cursor belongs to a different query. A cursor continues only the call that returned it, with the same arguments apart from `limit` and `max_tokens`. Call again without `cursor`."#,
        ),
        (
            "git_blame",
            json!({ "path": "src/lib.rs", "line": 0 }),
            r#"`git_blame`: lines are 1-based; accepted shapes: {path} (whole file), {path, line, end_line} (a 1-based inclusive span; `end_line` defaults to `line`), {path, symbol} (one symbol's lines; add `line` when several share the name); `rev` picks the revision"#,
        ),
        (
            "git_blame",
            json!({ "path": "src/lib.rs", "symbol": "alpha", "end_line": 2 }),
            r#"`git_blame`: `end_line` applies only to a line span; with `symbol`, `line` picks the definition; accepted shapes: {path} (whole file), {path, line, end_line} (a 1-based inclusive span; `end_line` defaults to `line`), {path, symbol} (one symbol's lines; add `line` when several share the name); `rev` picks the revision"#,
        ),
        (
            "git_blame",
            json!({ "path": "src/lib.rs", "line": 3, "end_line": 1 }),
            r#"`git_blame`: `end_line` must be >= `line` (got 3..1); accepted shapes: {path} (whole file), {path, line, end_line} (a 1-based inclusive span; `end_line` defaults to `line`), {path, symbol} (one symbol's lines; add `line` when several share the name); `rev` picks the revision"#,
        ),
        (
            "git_blame",
            json!({ "path": "src/lib.rs", "end_line": 2 }),
            r#"`git_blame`: `end_line` needs `line`; accepted shapes: {path} (whole file), {path, line, end_line} (a 1-based inclusive span; `end_line` defaults to `line`), {path, symbol} (one symbol's lines; add `line` when several share the name); `rev` picks the revision"#,
        ),
        (
            "git_blame",
            json!({ "path": "src/lib.rs", "line": 99 }),
            r#"`git_blame`: `src/lib.rs` has 7 lines at c48e32cbf7dde9e9c1a4ee2a1decb1560000d2d6; line 99 is out of range"#,
        ),
        (
            "git_blame",
            json!({ "path": "missing.rs", "line": 1 }),
            r#"`git_blame`: no file `missing.rs` at c48e32cbf7dde9e9c1a4ee2a1decb1560000d2d6"#,
        ),
        (
            "git_blame",
            json!({ "path": "src/lib.rs", "rev": "nope" }),
            r#"resolve_rev(nope): rev parse failed for nope: couldn't parse revision, input="nope""#,
        ),
        (
            "git_blame",
            json!({ "path": "src/lib.rs", "symbol": "nope" }),
            r#"`git_blame`: no symbol `nope` in `src/lib.rs` at HEAD; it defines: alpha (lines 1-3), beta (lines 5-7)"#,
        ),
        (
            "git_blame",
            json!({ "path": "notes/readme.txt", "symbol": "x" }),
            r#"`git_blame`: no symbol `x` in `notes/readme.txt` at HEAD; the file has no such symbols"#,
        ),
        (
            "git_blame",
            json!({ "path": "src/gamma.rs", "symbol": "gamma", "rev": "HEAD~2" }),
            r#"`git_blame`: `src/gamma.rs` does not exist at HEAD~2."#,
        ),
        (
            "git_blame",
            json!({ "path": "src/lib.rs", "limit": 0 }),
            r#"`git_blame`: `limit` must be at least 1; omit it for the default page size."#,
        ),
        (
            "git_blame",
            json!({ "path": "src/lib.rs", "cursor": "not-bound" }),
            r#"`git_blame`: this cursor belongs to a different query. A cursor continues only the call that returned it, with the same arguments apart from `limit` and `max_tokens`. Call again without `cursor`."#,
        ),
        (
            "git_diff",
            json!({ "from": "HEAD" }),
            r#"`git_diff`: a diff needs the `path` of one file; accepted shapes: {} (working-tree status), {path} ending in `/` (the status under that directory), {path} (that file's working-tree diff against HEAD), {path, from?, to?} (text diff; `from` defaults to HEAD, `to` to the working tree), {path, structural:true} with optional `from` (symbols added / removed since `from`, default HEAD)"#,
        ),
        (
            "git_diff",
            json!({ "structural": true }),
            r#"`git_diff`: a diff needs the `path` of one file; accepted shapes: {} (working-tree status), {path} ending in `/` (the status under that directory), {path} (that file's working-tree diff against HEAD), {path, from?, to?} (text diff; `from` defaults to HEAD, `to` to the working tree), {path, structural:true} with optional `from` (symbols added / removed since `from`, default HEAD)"#,
        ),
        (
            "git_diff",
            json!({ "path": "src/", "from": "HEAD" }),
            r#"`git_diff`: a directory `path` narrows the working-tree status and takes no `from`, `to` or `structural`; accepted shapes: {} (working-tree status), {path} ending in `/` (the status under that directory), {path} (that file's working-tree diff against HEAD), {path, from?, to?} (text diff; `from` defaults to HEAD, `to` to the working tree), {path, structural:true} with optional `from` (symbols added / removed since `from`, default HEAD)"#,
        ),
        (
            "git_diff",
            json!({ "path": "src" }),
            r#"`git_diff`: `src` is a directory and a diff is of one file. Pass `src/` (with the trailing slash, and no `from`, `to` or `structural`) for the working-tree status under it, or the path of a file in it."#,
        ),
        (
            "git_diff",
            json!({ "path": "src/lib.rs", "structural": true, "to": "HEAD" }),
            r#"`git_diff`: a structural diff compares `from` against the indexed working tree and takes no `to`; accepted shapes: {} (working-tree status), {path} ending in `/` (the status under that directory), {path} (that file's working-tree diff against HEAD), {path, from?, to?} (text diff; `from` defaults to HEAD, `to` to the working tree), {path, structural:true} with optional `from` (symbols added / removed since `from`, default HEAD)"#,
        ),
        (
            "git_diff",
            json!({ "path": "nope/" }),
            r#"`git_diff`: no directory `nope/` exists and no changed path is under it. `path` is repo-relative; a directory ends with `/`. Locate it with `find_files`."#,
        ),
        (
            "git_diff",
            json!({ "path": "src/lib.rs", "from": "nope" }),
            r#"`git_diff`: cannot resolve revision `nope`: rev parse failed for nope: couldn't parse revision, input="nope""#,
        ),
        (
            "git_diff",
            json!({ "path": "src/lib.rs", "from": "HEAD", "to": "nope" }),
            r#"`git_diff`: cannot resolve revision `nope`: rev parse failed for nope: couldn't parse revision, input="nope""#,
        ),
        (
            "git_diff",
            json!({ "path": "missing.rs" }),
            r#"`git_diff`: `missing.rs` exists neither at `HEAD` nor in the working tree. `path` is a repo-relative file path; locate it with `find_files`."#,
        ),
        (
            "git_diff",
            json!({ "path": "missing.rs", "structural": true }),
            r#"path not present in current view or at HEAD: missing.rs"#,
        ),
        (
            "git_diff",
            json!({ "path": "src/lib.rs", "structural": true, "from": "nope" }),
            r#"resolve_rev(nope): rev parse failed for nope: couldn't parse revision, input="nope""#,
        ),
    ];
    let mut rows = Vec::new();
    for (tool, arguments, expected) in cases {
        let actual = call_err(&client, tool, arguments.clone()).await;
        rows.push((format!("{tool} {arguments}"), actual, expected.to_string()));
    }
    assert_all(&rows);
}

/// A plain directory: every git tool says it needs a repository.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workspace_without_git_refuses_every_git_tool() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("lib.rs"), LIB_V1).unwrap();
    super::scan(dir.path());
    let client = super::serve(dir.path()).await;
    let mut rows = Vec::new();
    for (tool, arguments) in [
        ("git_log", json!({})),
        ("git_blame", json!({ "path": "lib.rs" })),
        ("git_diff", json!({})),
        ("git_diff", json!({ "path": "lib.rs" })),
    ] {
        let actual = call_err(&client, tool, arguments.clone()).await;
        rows.push((
            format!("{tool} {arguments}"),
            actual,
            "this tool requires `basemind serve` to be run inside a git repository".to_string(),
        ));
    }
    assert_all(&rows);
}

/// `basemind --json git <args>` over `root`: its JSON compacted with the per-run `elapsed_us` and
/// `startup_us` removed, or the `Error:` it printed.
fn cli(root: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .arg("--root")
        .arg(root)
        .arg("--json")
        .arg("git")
        .args(args)
        .output()
        .expect("run basemind");
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let at = stderr
            .find("Error: ")
            .unwrap_or_else(|| panic!("no error printed: {stderr}"));
        return stderr[at..].trim_end().to_string();
    }
    let mut value: Value = serde_json::from_slice(&out.stdout).expect("JSON on stdout");
    let object = value.as_object_mut().expect("an object");
    object.remove("elapsed_us");
    object.remove("startup_us");
    value.to_string()
}

/// The two git commands with no MCP tool: `churn` and `by-path`.
#[test]
fn git_churn_and_by_path_print_byte_for_byte() {
    let dir = dated_repo();
    super::scan(dir.path());
    let cases: [(&[&str], &str); 7] = [
        (
            &["churn"],
            r#"{"window_inspected":200,"total_files_changed":3,"files":[{"path":"src/lib.rs","commits_touching":2,"added":1,"modified":1,"deleted":0},{"path":"notes/readme.txt","commits_touching":1,"added":1,"modified":0,"deleted":0},{"path":"src/gamma.rs","commits_touching":1,"added":1,"modified":0,"deleted":0}]}"#,
        ),
        (
            &["churn", "--window", "2", "--top-k", "1"],
            r#"{"window_inspected":2,"total_files_changed":2,"files":[{"path":"src/gamma.rs","commits_touching":1,"added":1,"modified":0,"deleted":0}]}"#,
        ),
        (
            &["by-path", "^src/"],
            r#"{"window_inspected":3,"commits":[{"sha":"c48e32cbf7dd","summary":"add gamma","author":"t","author_time_unix":1704240000,"files":{"src/gamma.rs":"added"}},{"sha":"0c2fe19c38a2","summary":"tune alpha","author":"t","author_time_unix":1704153600,"files":{"src/lib.rs":"modified"}},{"sha":"3f38556b6d45","summary":"init","author":"t","author_time_unix":1704067200,"files":{"notes/readme.txt":"added","src/lib.rs":"added"}}]}"#,
        ),
        (
            &["by-path", "^src/", "--window", "1"],
            r#"{"window_inspected":1,"truncated":true,"truncated_reason":"window","commits":[{"sha":"c48e32cbf7dd","summary":"add gamma","author":"t","author_time_unix":1704240000,"files":{"src/gamma.rs":"added"}}],"next_cursor":"ks-AAAAAAAAAAc7EjjLL"}"#,
        ),
        (
            &["by-path", "^src/", "--limit", "0"],
            r#"Error: git_by_path: `git_by_path`: `limit` must be at least 1; omit it for the default page size."#,
        ),
        (
            &["by-path", "^src/", "--window", "0"],
            r#"Error: git_by_path: `git_by_path`: `window` must be at least 1; omit it for the default of 200 commits."#,
        ),
        (
            &["by-path", "("],
            r#"Error: git_by_path: invalid regex: regex parse error:
    (
    ^
error: unclosed group"#,
        ),
    ];
    let mut rows = Vec::new();
    for (args, expected) in cases {
        rows.push((format!("git {args:?}"), cli(dir.path(), args), expected.to_string()));
    }
    assert_all(&rows);
}
