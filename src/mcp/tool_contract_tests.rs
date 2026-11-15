//! The verb-tool contract on a one-shot server: an empty index is never an ordinary empty result,
//! and a failure names the next call to make.

use std::sync::Arc;

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

use super::BasemindServer;
use super::params::{
    CodeGrepParams, CodeOutlineParams, CodeReadParams, CodeSymbolsParams, FindFilesToolParams, parse_args,
};
use super::tool_contract::into_tool_result;
use crate::config::ConfigV1;
use crate::git_cache::GitCache;
use crate::scanner::{EmbedMode, ScanSource, scan};
use crate::store::{Store, VIEW_WORKING};

/// A one-shot server over `root`, scanned first when `scanned` is set.
fn server(root: &std::path::Path, scanned: bool) -> BasemindServer {
    crate::store::init_isolated_cache();
    let config = ConfigV1::with_defaults();
    {
        let mut store = Store::open(root, VIEW_WORKING).expect("open rw");
        if scanned {
            scan(root, &mut store, &config, ScanSource::WorkingTree, EmbedMode::Inline).expect("scan");
        } else {
            store.flush().expect("persist the empty file map");
        }
    }
    let store = Store::open_read_only(root, VIEW_WORKING).expect("open ro");
    let git_cache = Arc::new(GitCache::open(&store.basemind_dir, 16, false).expect("git cache"));
    BasemindServer::new_oneshot(store, root.to_path_buf(), Arc::new(config), None, git_cache)
}

fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|content| match content {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn json_of(result: &CallToolResult) -> Value {
    assert_ne!(
        result.is_error,
        Some(true),
        "unexpected tool error: {}",
        text_of(result)
    );
    serde_json::from_str(&text_of(result)).expect("tool payload is JSON")
}

fn symbols(name: &str) -> CodeSymbolsParams {
    parse_args(serde_json::json!({ "name": name })).expect("valid arguments")
}

fn outline(path: &str) -> CodeOutlineParams {
    parse_args(serde_json::json!({ "path": path })).expect("valid arguments")
}

#[tokio::test]
async fn an_empty_index_attaches_a_notice_to_every_list() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("a.rs"), "pub fn alpha() {}\n").expect("a.rs");
    let server = server(tmp.path(), false);

    let found = json_of(&server.code_symbols_call(symbols("alpha")).await.expect("code_symbols"));
    assert_eq!(found["total"], 0);
    assert_eq!(found["notice"]["state"], "empty_index", "{found}");
    assert!(
        found["notice"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("index_rescan")),
        "the notice must name the fix: {found}"
    );

    let grep: CodeGrepParams = parse_args(serde_json::json!({ "pattern": "alpha" })).expect("valid arguments");
    let grepped = json_of(&server.code_grep_call(grep).await.expect("code_grep"));
    assert_eq!(grepped["notice"]["state"], "empty_index", "{grepped}");

    let files: FindFilesToolParams = parse_args(serde_json::json!({})).expect("valid arguments");
    let listed = json_of(&server.find_files_call(files).await.expect("find_files"));
    assert_eq!(listed["notice"]["state"], "empty_index", "{listed}");
}

#[tokio::test]
async fn a_path_tool_on_an_empty_index_is_an_error_naming_the_rescan() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("a.rs"), "pub fn alpha() {}\n").expect("a.rs");
    let server = server(tmp.path(), false);

    let result = into_tool_result(server.code_outline_call(outline("a.rs")).await);
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    assert!(text.contains("the index is empty"), "{text}");
    assert!(text.contains("index_rescan"), "{text}");
}

#[tokio::test]
async fn a_populated_index_carries_no_empty_index_notice() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("a.rs"), "pub fn alpha() {}\n").expect("a.rs");
    let server = server(tmp.path(), true);

    let found = json_of(&server.code_symbols_call(symbols("alpha")).await.expect("code_symbols"));
    assert_eq!(found["total"], 1, "{found}");
    assert!(found.get("notice").is_none(), "{found}");
}

#[tokio::test]
async fn a_file_that_is_not_indexed_says_what_to_do_about_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("a.rs"), "pub fn alpha() {}\n").expect("a.rs");
    let server = server(tmp.path(), true);

    std::fs::write(tmp.path().join("late.rs"), "pub fn late() {}\n").expect("late.rs");
    let unscanned = text_of(&into_tool_result(server.code_outline_call(outline("late.rs")).await));
    assert!(unscanned.contains("exists but is not indexed"), "{unscanned}");
    assert!(
        unscanned.contains("index_rescan {\"paths\":[\"late.rs\"]}"),
        "{unscanned}"
    );

    let missing = text_of(&into_tool_result(server.code_outline_call(outline("nope.rs")).await));
    assert!(missing.contains("no file `nope.rs`"), "{missing}");
    assert!(missing.contains("find_files"), "{missing}");

    let read: CodeReadParams =
        parse_args(serde_json::json!({ "path": "nope.rs", "symbol": "x" })).expect("valid arguments");
    let unread = into_tool_result(server.code_read_call(read).await);
    assert_eq!(unread.is_error, Some(true));
    assert!(text_of(&unread).contains("find_files"));
}

/// The three tools that report a symbol's position must agree on it: an outline span is what
/// `code_read` returns and what `git_blame` blames.
#[tokio::test]
async fn outline_lines_are_one_based_and_agree_with_code_read() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("a.rs"),
        "// header\n\npub fn alpha() {\n    let _ = 1;\n}\n\npub fn beta() {}\n",
    )
    .expect("a.rs");
    let server = server(tmp.path(), true);

    let outlined = json_of(&server.code_outline_call(outline("a.rs")).await.expect("code_outline"));
    let alpha = outlined["symbols"]
        .as_array()
        .expect("symbols")
        .iter()
        .find(|symbol| symbol["name"] == "alpha")
        .expect("alpha in the outline")
        .clone();
    assert_eq!(
        (alpha["line"].as_u64(), alpha["end_line"].as_u64()),
        (Some(3), Some(5)),
        "{alpha}"
    );
    assert!(
        alpha.get("start_row").is_none(),
        "the 0-based row must be gone: {alpha}"
    );

    let by_symbol: CodeReadParams =
        parse_args(serde_json::json!({ "path": "a.rs", "symbol": "alpha" })).expect("valid arguments");
    let body = json_of(&server.code_read_call(by_symbol).await.expect("code_read symbol"));
    assert_eq!(
        (body["line"].as_u64(), body["end_line"].as_u64()),
        (Some(3), Some(5)),
        "{body}"
    );

    let by_span: CodeReadParams =
        parse_args(serde_json::json!({ "path": "a.rs", "line": 3, "end_line": 5 })).expect("valid arguments");
    let span = json_of(&server.code_read_call(by_span).await.expect("code_read span"));
    assert_eq!(
        span["body"], body["body"],
        "the outline span must read back the symbol's body"
    );
    assert_eq!(span["body"], "pub fn alpha() {\n    let _ = 1;\n}");
}

#[tokio::test]
async fn grep_is_literal_unless_regex_is_set() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("a.rs"),
        "pub fn cache_root() {}\npub fn caller() { cache_root(); }\n",
    )
    .expect("a.rs");
    let server = server(tmp.path(), true);

    let literal: CodeGrepParams = parse_args(serde_json::json!({ "pattern": "cache_root(" })).expect("valid arguments");
    let hits = json_of(
        &server
            .code_grep_call(literal)
            .await
            .expect("a literal `(` is not a regex error"),
    );
    assert_eq!(hits["total_matches"], 2, "{hits}");
    assert_eq!(
        hits["pattern"], "cache_root(",
        "the response echoes what the caller typed"
    );

    let as_regex: CodeGrepParams =
        parse_args(serde_json::json!({ "pattern": "cache_root(", "regex": true })).expect("valid arguments");
    let refused = into_tool_result(server.code_grep_call(as_regex).await);
    assert_eq!(refused.is_error, Some(true));
    assert!(text_of(&refused).contains("invalid regex"), "{}", text_of(&refused));

    let regex: CodeGrepParams =
        parse_args(serde_json::json!({ "pattern": "fn c\\w+r\\b", "regex": true })).expect("valid arguments");
    let matched = json_of(&server.code_grep_call(regex).await.expect("regex grep"));
    assert_eq!(matched["total_matches"], 1, "{matched}");

    let dotted: CodeGrepParams = parse_args(serde_json::json!({ "pattern": "c.che_root" })).expect("valid arguments");
    let none = json_of(&server.code_grep_call(dotted).await.expect("literal grep"));
    assert_eq!(
        none["total_matches"], 0,
        "a literal `.` must not match any character: {none}"
    );

    let glob: CodeGrepParams =
        parse_args(serde_json::json!({ "pattern": "cache_root", "path": "*.rs" })).expect("valid arguments");
    let not_a_glob = text_of(&into_tool_result(server.code_grep_call(glob).await));
    assert!(not_a_glob.contains("not a glob"), "{not_a_glob}");
}
