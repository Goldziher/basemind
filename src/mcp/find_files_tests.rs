//! `find_files` covers every indexed path: the code map UNION the document tier, filtered by
//! language, cut at a relative score and in a deterministic order.

use std::sync::Arc;

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

use super::BasemindServer;
use super::params::FindFilesToolParams;
use crate::config::ConfigV1;
use crate::git_cache::GitCache;
use crate::scanner::{EmbedMode, ScanSource, scan};
use crate::store::{Store, VIEW_WORKING};

fn server(root: &std::path::Path) -> BasemindServer {
    crate::store::init_isolated_cache();
    for (name, body) in [
        ("widget_factory.rs", "pub fn make() {}\n"),
        (
            "README.md",
            "# Readme\n\nThe widget guide, long enough to be chunked as a document.\n",
        ),
        ("deploy_settings.yaml", "replicas: 3\nregion: eu\n"),
        ("package_manifest.json", "{\"name\": \"x\", \"version\": \"1\"}\n"),
        ("unrelated_thing.rs", "pub fn other() {}\n"),
    ] {
        std::fs::write(root.join(name), body).expect("write fixture");
    }
    let mut config = ConfigV1::with_defaults();
    config.documents.enabled = true;
    config.documents.embed = false;
    // The document tier's LanceDB writes `block_on` internally, which panics inside the test's
    // tokio runtime; scan on a plain thread.
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut store = Store::open(root, VIEW_WORKING).expect("open rw");
            scan(root, &mut store, &config, ScanSource::WorkingTree, EmbedMode::Inline).expect("scan");
        })
        .join()
        .expect("scan thread");
    });
    let store = Store::open_read_only(root, VIEW_WORKING).expect("open ro");
    let git_cache = Arc::new(GitCache::open(&store.basemind_dir, 16, false).expect("git cache"));
    BasemindServer::new_oneshot(store, root.to_path_buf(), Arc::new(config), None, git_cache)
}

fn json_of(result: &CallToolResult) -> Value {
    for content in &result.content {
        if let ContentBlock::Text(text) = content {
            return serde_json::from_str(&text.text).expect("tool payload is JSON");
        }
    }
    panic!("tool returned no text content");
}

fn listing(query: Option<&str>, language: Option<&str>) -> FindFilesToolParams {
    FindFilesToolParams {
        query: query.map(str::to_string),
        path_prefix: None,
        language: language.map(str::to_string),
        limit: None,
        cursor: None,
        max_tokens: None,
    }
}

/// The paths of a `find_files` page, which lists each file by its path alone.
fn paths(payload: &Value) -> Vec<String> {
    payload["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f.as_str().expect("a file is its path").to_string())
        .collect()
}

async fn find(server: &BasemindServer, query: &str, language: Option<&str>) -> Vec<String> {
    let result = server
        .find_files_call(listing(Some(query), language))
        .await
        .expect("find");
    paths(&json_of(&result))
}

#[tokio::test]
async fn find_covers_code_and_documents_and_filters_by_language() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let server = server(tmp.path());

    for (query, path) in [
        ("README.md", "README.md"),
        ("deploy_settings.yaml", "deploy_settings.yaml"),
        ("package_manifest", "package_manifest.json"),
        ("widget_factory", "widget_factory.rs"),
    ] {
        let found = find(&server, query, None).await;
        assert_eq!(found.first().map(String::as_str), Some(path), "{query}: {found:?}");
    }

    // The language filter applies to the document label too.
    assert_eq!(find(&server, "deploy", Some("yaml")).await, ["deploy_settings.yaml"]);
    assert!(find(&server, "deploy", Some("rust")).await.is_empty());
    assert_eq!(find(&server, "widget", Some("rust")).await, ["widget_factory.rs"]);
}

/// A transposed letter matches nothing as a subsequence; the file it names is still found first.
#[tokio::test]
async fn find_names_the_file_a_transposed_or_dropped_letter_meant() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let server = server(tmp.path());
    for (typo, meant) in [
        ("wdiget_factory", "widget_factory.rs"),
        ("deploy_setings", "deploy_settings.yaml"),
        ("REDAME", "README.md"),
    ] {
        let found = find(&server, typo, None).await;
        assert_eq!(found.first().map(String::as_str), Some(meant), "{typo}: {found:?}");
    }
}

#[tokio::test]
async fn find_cuts_weak_matches_and_is_deterministic() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let server = server(tmp.path());
    let first = find(&server, "deploy_settings", None).await;
    assert_eq!(
        first,
        ["deploy_settings.yaml"],
        "weak subsequence matches must be cut: {first:?}"
    );
    for _ in 0..3 {
        assert_eq!(find(&server, "deploy_settings", None).await, first);
        assert_eq!(find(&server, "e", None).await, find(&server, "e", None).await);
    }
}

#[tokio::test]
async fn files_lists_documents_alongside_code() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let server = server(tmp.path());
    let result = server
        .find_files_call(listing(None, Some("markdown")))
        .await
        .expect("files");
    let payload = json_of(&result);
    assert_eq!(paths(&payload), ["README.md"]);
    assert_eq!(payload["total"], 1, "{payload}");
    let keys: Vec<&String> = payload.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        ["total", "files"],
        "a whole listing carries nothing else: {payload}"
    );
}
