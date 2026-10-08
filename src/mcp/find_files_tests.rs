//! `find` / `files` cover every indexed path: the code map UNION the document tier, with a
//! language label for documents, a relative-score cutoff and a deterministic order.

use std::sync::Arc;

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

use super::BasemindServer;
use super::mode::CodeMode;
use super::params::{Lenient, Parameters};
use super::types_code::CodeParams;
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

async fn find(server: &BasemindServer, query: &str, language: Option<&str>) -> Vec<(String, String)> {
    let params = CodeParams {
        query: Some(query.to_string()),
        language: language.map(str::to_string),
        ..CodeParams::new(CodeMode::Find)
    };
    let result = server.code(Parameters(Lenient(params))).await.expect("find");
    json_of(&result)["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| {
            (
                f["path"].as_str().unwrap().to_string(),
                f["language"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn find_covers_code_and_documents_with_language_labels() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let server = server(tmp.path());

    let readme = find(&server, "README.md", None).await;
    assert_eq!(
        readme.first(),
        Some(&("README.md".to_string(), "markdown".to_string())),
        "{readme:?}"
    );
    let yaml = find(&server, "deploy_settings.yaml", None).await;
    assert_eq!(yaml.first().map(|f| f.1.as_str()), Some("yaml"), "{yaml:?}");
    let json = find(&server, "package_manifest", None).await;
    assert_eq!(
        json.first().map(|f| f.0.as_str()),
        Some("package_manifest.json"),
        "{json:?}"
    );
    let code = find(&server, "widget_factory", None).await;
    assert_eq!(
        code.first(),
        Some(&("widget_factory.rs".to_string(), "rust".to_string())),
        "{code:?}"
    );

    // The language filter applies to the document label too.
    let only_yaml = find(&server, "deploy", Some("yaml")).await;
    assert_eq!(only_yaml.len(), 1, "{only_yaml:?}");
    assert!(find(&server, "deploy", Some("rust")).await.is_empty());
}

#[tokio::test]
async fn find_cuts_weak_matches_and_is_deterministic() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let server = server(tmp.path());
    let first = find(&server, "deploy_settings", None).await;
    let paths: Vec<_> = first.iter().map(|f| f.0.as_str()).collect();
    assert_eq!(
        paths,
        ["deploy_settings.yaml"],
        "weak subsequence matches must be cut: {paths:?}"
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
    let params = CodeParams {
        language: Some("markdown".to_string()),
        ..CodeParams::new(CodeMode::Files)
    };
    let result = server.code(Parameters(Lenient(params))).await.expect("files");
    let payload = json_of(&result);
    let paths: Vec<&str> = payload["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["path"].as_str())
        .collect();
    assert_eq!(paths, ["README.md"]);
    assert!(payload["files"][0]["size_bytes"].as_u64().unwrap() > 0);
}
