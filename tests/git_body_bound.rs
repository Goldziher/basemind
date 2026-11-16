//! `git_log` responses stay bounded when commit messages are huge (GH #74).
//!
//! A squash-merge commit can carry a body of hundreds of kilobytes; a commit search used to return
//! every hit's body whole, so one call at `limit` 100 cost megabytes. These tests build a repo with
//! large-body commits and pin the three bounds: the per-hit body cap, the token budget on every
//! commit list (`max_tokens`, with `next_cursor` resuming at the first dropped commit), and
//! `git_log {rev}` as the way back to the whole message.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
use tempfile::TempDir;

const TOKEN: &str = "zzbodyboundtoken";
const BIG_BODY_BYTES: usize = 200_000;
const COMMITS: usize = 4;

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

/// Four commits, each touching `a.rs` and `dir/b.rs` and carrying [`TOKEN`] in a ~200 KB body.
fn build_repo() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    for i in 0..COMMITS {
        std::fs::write(root.join("a.rs"), format!("pub fn alpha() -> u32 {{ {i} }}\n")).expect("write a.rs");
        std::fs::create_dir_all(root.join("dir")).expect("create dir");
        std::fs::write(root.join("dir/b.rs"), format!("pub fn beta() -> u32 {{ {i} }}\n")).expect("write dir/b.rs");
        git(root, &["add", "."]);
        let body = format!("{TOKEN} {}", "lorem ipsum dolor ".repeat(BIG_BODY_BYTES / 18));
        // The body is far past MAX_ARG_STRLEN (128 KiB on Linux), so it goes through a file, not argv.
        let message = root.join(".git").join("COMMIT_BODY");
        std::fs::write(&message, format!("squash {i}\n\n{body}\n")).expect("write message");
        git(root, &["commit", "-q", "-F", message.to_str().expect("utf8 path")]);
    }
    dir
}

fn decode(result: &CallToolResult) -> Value {
    let raw = result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or(Value::Null)
}

fn call_params(args: Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new("git_log");
    if let Some(obj) = args.as_object() {
        params = params.with_arguments(obj.clone());
    }
    params
}

type Session = rmcp::service::RunningService<rmcp::RoleClient, ()>;

async fn spawn(root: &Path) -> Session {
    let transport = basemind::mcp::serve_in_memory(root, "working")
        .await
        .expect("in-memory serve");
    ().serve(transport).await.expect("rmcp handshake")
}

/// Call `git_log`, returning the decoded body and the serialized size in bytes.
async fn call(service: &Session, args: Value) -> (Value, usize) {
    let result = service
        .call_tool(call_params(args.clone()))
        .await
        .expect("git_log call");
    assert_ne!(result.is_error, Some(true), "git_log {args} failed: {result:?}");
    let size = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.len()))
        .sum();
    (decode(&result), size)
}

/// Search until the git-history index answers (`truncated` unset): bodies exist only there.
async fn indexed_search(service: &Session, extra: Value) -> (Value, usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let mut args = json!({ "query": TOKEN, "limit": 100 });
        args.as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        let (value, size) = call(service, args).await;
        let indexed = !value.get("truncated").and_then(Value::as_bool).unwrap_or(false);
        if indexed
            && value
                .get("commits")
                .and_then(Value::as_array)
                .is_some_and(|c| !c.is_empty())
        {
            return (value, size);
        }
        assert!(std::time::Instant::now() < deadline, "index never answered: {value}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn commits(value: &Value) -> &Vec<Value> {
    value.get("commits").and_then(Value::as_array).expect("commits array")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_caps_each_body_and_marks_the_cut() {
    let dir = build_repo();
    let service = spawn(dir.path()).await;

    let (value, size) = indexed_search(&service, json!({ "max_tokens": 1_000_000 })).await;
    let hits = commits(&value);
    assert_eq!(hits.len(), COMMITS, "{value}");
    assert!(
        size < 20_000,
        "{COMMITS} hits with ~{BIG_BODY_BYTES}-byte bodies must stay small, got {size} bytes"
    );
    for hit in hits {
        let body = hit["body"].as_str().expect("body");
        assert!(body.starts_with(TOKEN), "{body}");
        let short_sha = hit["short_sha"].as_str().expect("short_sha");
        assert!(
            body.contains("body truncated") && body.contains(short_sha),
            "marker must name the commit to ask for in full: {body}"
        );
        assert_eq!(hit["body_truncated"], json!(true));
        assert!(hit["body_bytes"].as_u64().expect("body_bytes") > BIG_BODY_BYTES as u64 - 100);
        assert!(
            body.chars().count() < 1_300,
            "default cap is about 1000 chars: {}",
            body.len()
        );
    }

    let _ = service.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_budget_bounds_the_response_and_pages_on() {
    let dir = build_repo();
    let service = spawn(dir.path()).await;

    // A one-token budget keeps exactly the first hit and hands back a cursor to the rest.
    let (first, _) = indexed_search(&service, json!({ "max_tokens": 1 })).await;
    assert_eq!(commits(&first).len(), 1, "{first}");
    assert_eq!(first["budgeted"], json!(true));
    let mut seen = vec![commits(&first)[0]["sha"].as_str().expect("sha").to_string()];
    let mut cursor = first["next_cursor"].clone();
    assert!(cursor.is_string(), "a budgeted page must carry next_cursor: {first}");

    while cursor.is_string() {
        let (page, _) = call(&service, json!({ "query": TOKEN, "max_tokens": 1, "cursor": cursor })).await;
        let page_commits = commits(&page);
        assert_eq!(page_commits.len(), 1, "{page}");
        let sha = page_commits[0]["sha"].as_str().expect("sha").to_string();
        assert!(!seen.contains(&sha), "pages must not overlap: {sha}");
        seen.push(sha);
        cursor = page.get("next_cursor").cloned().unwrap_or(Value::Null);
    }
    assert_eq!(seen.len(), COMMITS, "paging must reach every hit exactly once");

    let _ = service.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn show_returns_the_whole_message_and_files() {
    let dir = build_repo();
    let service = spawn(dir.path()).await;

    let (value, _) = indexed_search(&service, json!({ "max_tokens": 1_000_000 })).await;
    let sha = commits(&value)[0]["sha"].as_str().expect("sha").to_string();

    let (shown, size) = call(&service, json!({ "rev": sha })).await;
    let commit = &shown["commit"];
    assert_eq!(commit["sha"], json!(sha));
    let body = commit["body"].as_str().expect("body");
    assert!(
        body.len() >= BIG_BODY_BYTES - 100,
        "`rev` must not truncate: {} bytes",
        body.len()
    );
    assert!(commit.get("body_truncated").is_none());
    assert!(size >= BIG_BODY_BYTES - 100);
    let files = shown["files"].as_array().expect("files");
    assert!(files.iter().any(|file| file["path"] == json!("a.rs")), "{files:?}");

    // A full sha and a symbolic revision both resolve.
    let (by_rev, _) = call(&service, json!({ "rev": "HEAD" })).await;
    assert_eq!(by_rev["commit"]["summary"], json!(format!("squash {}", COMMITS - 1)));

    let _ = service.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commit_lists_honour_max_tokens() {
    let dir = build_repo();
    let service = spawn(dir.path()).await;

    for args in [
        json!({ "max_tokens": 1 }),
        json!({ "path": "a.rs", "max_tokens": 1 }),
        json!({ "path": "dir/", "max_tokens": 1 }),
    ] {
        let (value, _) = call(&service, args.clone()).await;
        assert_eq!(commits(&value).len(), 1, "{args}: {value}");
        assert_eq!(value["budgeted"], json!(true), "{args}: {value}");
        assert!(value["next_cursor"].is_string(), "{args}: {value}");
    }

    // Without a budget pressure nothing is dropped and `budgeted` is absent.
    let (value, _) = call(&service, json!({})).await;
    assert_eq!(commits(&value).len(), COMMITS);
    assert!(
        value.get("budgeted").is_none() && value.get("next_cursor").is_none(),
        "{value}"
    );

    let _ = service.cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_fields_and_conflicting_shapes_are_refused() {
    let dir = build_repo();
    let service = spawn(dir.path()).await;

    for (args, expected) in [
        (json!({ "body_chars": 10 }), "unknown field `body_chars`"),
        (json!({ "mode": "recent" }), "unknown field `mode`"),
        (
            json!({ "rev": "HEAD", "limit": 1 }),
            "`rev` shows one commit and takes no `limit` or `cursor`",
        ),
        (
            json!({ "rev": "HEAD", "path": "a.rs" }),
            "`rev` shows one commit and combines with no other filter",
        ),
    ] {
        let result = service
            .call_tool(call_params(args.clone()))
            .await
            .expect("a refusal is a tool result, not a protocol error");
        assert_eq!(result.is_error, Some(true), "{args} must be refused: {result:?}");
        let text = result
            .content
            .iter()
            .find_map(|c| c.as_text().map(|t| t.text.clone()))
            .unwrap_or_default();
        assert!(text.contains(expected), "{args}: {text}");
    }

    let _ = service.cancel().await;
}
