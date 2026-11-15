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
    server_with(root, scanned, ConfigV1::with_defaults())
}

fn server_with(root: &std::path::Path, scanned: bool, config: ConfigV1) -> BasemindServer {
    crate::store::init_isolated_cache();
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

/// A file the map lists whose blob cannot be read is indexed, over a cache entry that is lost. That
/// is a different answer from "not indexed", and the rescan it names must be the one that fixes it.
#[tokio::test]
async fn an_indexed_file_whose_blob_is_unreadable_says_so_and_the_rescan_it_names_rebuilds_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("a.rs"), "pub fn unreadable_blob_fixture() {}\n").expect("a.rs");
    std::fs::write(tmp.path().join("b.rs"), "pub fn beta() {}\n").expect("b.rs");
    let config = ConfigV1::with_defaults();
    crate::store::init_isolated_cache();
    {
        let mut store = Store::open(tmp.path(), VIEW_WORKING).expect("open rw");
        scan(
            tmp.path(),
            &mut store,
            &config,
            ScanSource::WorkingTree,
            EmbedMode::Inline,
        )
        .expect("scan");
        let hash_hex = store.lookup("a.rs").expect("entry").hash_hex.clone();
        let blob = store.blob_path_fm_hex(&hash_hex);
        let whole = std::fs::read(&blob).expect("read blob");
        std::fs::write(&blob, &whole[..whole.len() / 2]).expect("tear the blob");
    }
    let store = Store::open_read_only(tmp.path(), VIEW_WORKING).expect("open ro");
    let git_cache = Arc::new(GitCache::open(&store.basemind_dir, 16, false).expect("git cache"));
    let server = BasemindServer::new_oneshot(store, tmp.path().to_path_buf(), Arc::new(config), None, git_cache);

    let refused = into_tool_result(server.code_outline_call(outline("a.rs")).await);

    assert_eq!(refused.is_error, Some(true));
    let text = text_of(&refused);
    assert!(text.contains("missing or damaged"), "{text}");
    assert!(text.contains(r#"index_rescan {"paths":["a.rs"]}"#), "{text}");
    assert!(!text.contains("is not indexed"), "{text}");

    // What `index_rescan {"paths":["a.rs"]}` runs.
    let named = Some(vec![tmp.path().join("a.rs")]);
    super::helpers::scan_and_refresh(Arc::clone(&server.state), named, EmbedMode::Deferred, true)
        .await
        .expect("rescan the named path");
    let served = into_tool_result(server.code_outline_call(outline("a.rs")).await);
    assert_ne!(served.is_error, Some(true), "{}", text_of(&served));
    assert!(text_of(&served).contains("unreadable_blob_fixture"));
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

#[cfg(unix)]
const LINKED_RS: &str = "pub fn linked_needle() {}\n";

/// A repository with `linked.rs` pointing at a file in a sibling directory, and that directory.
#[cfg(unix)]
fn repo_with_outside_link() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).expect("repo");
    std::fs::create_dir_all(tmp.path().join("outside")).expect("outside");
    std::fs::write(root.join("a.rs"), "pub fn alpha() {}\n").expect("a.rs");
    std::fs::write(tmp.path().join("outside/linked.rs"), LINKED_RS).expect("linked.rs");
    (tmp, root)
}

/// With `scan.follow_symlinks` the scan indexes a link whose target is outside the root, and
/// `code_grep` reads it. The tools that read one indexed file used to refuse the same path, so a
/// grep hit could not be opened.
#[cfg(unix)]
#[tokio::test]
async fn a_followed_symlink_the_scan_indexed_is_readable_by_every_tool() {
    let (tmp, root) = repo_with_outside_link();
    std::os::unix::fs::symlink(tmp.path().join("outside/linked.rs"), root.join("linked.rs")).expect("symlink");
    let mut config = ConfigV1::with_defaults();
    config.scan.follow_symlinks = true;
    let server = server_with(&root, true, config);

    let grep: CodeGrepParams = parse_args(serde_json::json!({ "pattern": "linked_needle" })).expect("valid arguments");
    let hits = json_of(&server.code_grep_call(grep).await.expect("code_grep"));
    assert_eq!(
        hits["hits"][0]["path"], "linked.rs",
        "the scan indexed the link: {hits}"
    );

    let read: CodeReadParams =
        parse_args(serde_json::json!({ "path": "linked.rs", "symbol": "linked_needle" })).expect("valid arguments");
    let body = json_of(&into_tool_result(server.code_read_call(read).await));
    assert_eq!(body["body"], "pub fn linked_needle() {}", "{body}");

    let span: CodeReadParams =
        parse_args(serde_json::json!({ "path": "linked.rs", "line": 1 })).expect("valid arguments");
    let line = json_of(&into_tool_result(server.code_read_call(span).await));
    assert_eq!(line["body"], "pub fn linked_needle() {}", "{line}");

    let outlined = json_of(&into_tool_result(server.code_outline_call(outline("linked.rs")).await));
    assert_eq!(outlined["symbols"][0]["name"], "linked_needle", "{outlined}");
    assert!(outlined.get("stale").is_none(), "the file was read: {outlined}");

    // The grant covers what the scan indexed, not every link in the tree.
    std::os::unix::fs::symlink(tmp.path().join("outside/linked.rs"), root.join("late.rs")).expect("symlink");
    let unindexed: CodeReadParams =
        parse_args(serde_json::json!({ "path": "late.rs", "line": 1 })).expect("valid arguments");
    let refused = into_tool_result(server.code_read_call(unindexed).await);
    assert_eq!(refused.is_error, Some(true));
    assert!(!text_of(&refused).contains("linked_needle"), "{}", text_of(&refused));
}

/// Without the grant, an indexed file replaced by a link out of the repository is not read by any
/// tool. `code_outline` derives its line spans from the file, so it refuses like `code_read`.
#[cfg(unix)]
#[tokio::test]
async fn an_indexed_file_replaced_by_an_outside_link_is_neither_outlined_nor_read() {
    let (tmp, root) = repo_with_outside_link();
    std::fs::write(root.join("linked.rs"), "pub fn indexed_before() {}\n").expect("linked.rs");
    let server = server(&root, true);
    std::fs::remove_file(root.join("linked.rs")).expect("remove");
    std::os::unix::fs::symlink(tmp.path().join("outside/linked.rs"), root.join("linked.rs")).expect("symlink");

    let outlined = into_tool_result(server.code_outline_call(outline("linked.rs")).await);
    assert_eq!(outlined.is_error, Some(true));
    let text = text_of(&outlined);
    assert!(text.contains("outside this repository"), "{text}");
    assert!(!text.contains("linked_needle"), "{text}");

    for arguments in [
        serde_json::json!({ "path": "linked.rs", "line": 1 }),
        serde_json::json!({ "path": "linked.rs", "symbol": "indexed_before" }),
    ] {
        let read: CodeReadParams = parse_args(arguments).expect("valid arguments");
        let refused = into_tool_result(server.code_read_call(read).await);
        assert_eq!(refused.is_error, Some(true));
        let text = text_of(&refused);
        assert!(text.contains("outside this repository"), "{text}");
        assert!(!text.contains("linked_needle"), "{text}");
    }
}
