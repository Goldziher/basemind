//! `find_files` covers every file of the workspace: the code map UNION the document tier UNION the
//! files git tracks that neither holds, filtered by language, cut at a relative score and in a
//! deterministic order.

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
    server_with(root, None)
}

/// A server over the fixture at `root`, answering with `repo` as its git repository.
fn server_with(root: &std::path::Path, repo: Option<Arc<crate::git::Repo>>) -> BasemindServer {
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
            scan(
                root,
                &mut store,
                &config,
                ScanSource::WorkingTree,
                EmbedMode::Inline,
                crate::retrieval_wiring::backend(),
            )
            .expect("scan");
        })
        .join()
        .expect("scan thread");
    });
    let store = Store::open_read_only(root, VIEW_WORKING).expect("open ro");
    let git_cache = Arc::new(GitCache::open(&store.basemind_dir, 16, false).expect("git cache"));
    BasemindServer::new_oneshot(store, root.to_path_buf(), Arc::new(config), repo, git_cache)
}

fn json_of(result: &CallToolResult) -> Value {
    for content in &result.content {
        if let ContentBlock::Text(text) = content {
            return crate::wire::decode(&text.text).expect("tool payload reads back");
        }
    }
    panic!("tool returned no text content");
}

fn listing(query: Option<&str>, language: Option<&str>) -> FindFilesToolParams {
    FindFilesToolParams {
        query: query.map(str::to_string),
        path: None,
        language: language.map(str::to_string),
        limit: None,
        cursor: None,
        max_tokens: None,
        min_lines: None,
        changed: None,
        indexed: None,
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

/// The path of every row of a page, whether the row is a bare path or a `{path, ...}` object.
fn paths_or_rows(payload: &Value) -> Vec<String> {
    payload["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f.as_str().or_else(|| f["path"].as_str()).expect("a path").to_string())
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

/// A file git tracks that the scan does not parse (an image, a font) is still a file to find, and to
/// list; it has no language, so a language filter leaves it out.
#[tokio::test]
async fn find_and_list_cover_files_git_tracks_that_are_not_indexed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let git = |args: &[&str]| {
        let status = crate::testing::git_command()
            .args(args)
            .current_dir(root)
            .status()
            .expect("git in PATH");
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::write(root.join("assets/banner.png"), [0x89, b'P', b'N', b'G', 0, 1, 2]).unwrap();
    std::fs::write(root.join("widget_factory.rs"), "pub fn make() {}\n").unwrap();
    git(&["add", "-A"]);
    let repo = Arc::new(crate::git::Repo::discover(root).expect("repo"));
    let server = server_with(root, Some(repo));

    assert_eq!(find(&server, "banner.png", None).await, ["assets/banner.png"]);
    assert_eq!(
        find(&server, "baner", None).await,
        ["assets/banner.png"],
        "a typo of it too"
    );
    assert!(find(&server, "banner", Some("rust")).await.is_empty());

    let listed = server.find_files_call(listing(None, None)).await.expect("files");
    assert!(paths(&json_of(&listed)).contains(&"assets/banner.png".to_string()));

    // A tracked file that is neither indexed nor list-only belongs to no `indexed` group.
    for (filter, query) in [
        (Some(crate::engine::types::files::IndexedFilter::Flag(true)), None),
        (
            Some(crate::engine::types::files::IndexedFilter::Flag(true)),
            Some("banner"),
        ),
        (Some(crate::engine::types::files::IndexedFilter::Flag(false)), None),
        (
            Some(crate::engine::types::files::IndexedFilter::Flag(false)),
            Some("banner"),
        ),
    ] {
        let mut params = listing(query, None);
        params.indexed = filter;
        let result = server.find_files_call(params).await.expect("files");
        assert!(
            !paths_or_rows(&json_of(&result)).contains(&"assets/banner.png".to_string()),
            "{filter:?} {query:?}"
        );
    }
    let mut params = listing(None, None);
    params.indexed = Some(crate::engine::types::files::IndexedFilter::Word(
        crate::engine::types::files::IndexedWord::Any,
    ));
    let any = server.find_files_call(params).await.expect("files");
    assert!(paths_or_rows(&json_of(&any)).contains(&"assets/banner.png".to_string()));
    let rust = server
        .find_files_call(listing(None, Some("rust")))
        .await
        .expect("files");
    assert!(!paths(&json_of(&rust)).contains(&"assets/banner.png".to_string()));
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
    let keys: Vec<&String> = payload.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        ["files"],
        "a whole listing carries nothing else (no `total` without `next_cursor`): {payload}"
    );
}

/// A lockfile is known to exist: `find_files` lists it with `indexed: false` and its kind, `indexed`
/// selects either group, and the file is not git-tracked-but-unindexed twice.
#[tokio::test]
async fn find_lists_list_only_files_with_their_kind_and_filters_by_indexed() {
    use crate::engine::types::files::{IndexedFilter, IndexedWord};

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::create_dir_all(root.join("infra")).unwrap();
    let lock_body = "provider \"x\" {}\n";
    std::fs::write(root.join("infra/.terraform.lock.hcl"), lock_body).unwrap();
    std::fs::write(root.join("capture.pcap"), [0xd4, 0xc3, 0xb2, 0xa1, 0, 0]).unwrap();
    let server = server(root);

    let rows = |filter: Option<IndexedFilter>, query: Option<&str>| {
        let mut params = listing(query, None);
        params.indexed = filter;
        let server = &server;
        async move {
            let result = server.find_files_call(params).await.expect("files");
            json_of(&result)["files"].as_array().expect("files").clone()
        }
    };

    let any = rows(None, None).await;
    let lock = any
        .iter()
        .find(|row| row["path"] == "infra/.terraform.lock.hcl")
        .unwrap_or_else(|| panic!("the lockfile is listed: {any:?}"));
    assert_eq!(lock["indexed"], false);
    assert_eq!(lock["kind"], "lockfile");
    assert_eq!(lock["bytes"], lock_body.len());
    assert!(
        any.iter()
            .any(|row| row["path"] == "capture.pcap" && row["kind"] == "data")
    );
    assert!(
        any.iter().any(|row| row == "widget_factory.rs"),
        "indexed files still list"
    );

    let only_listed = rows(Some(IndexedFilter::Flag(false)), None).await;
    assert_eq!(only_listed.len(), 2, "{only_listed:?}");
    assert!(only_listed.iter().all(|row| row["indexed"] == false));

    let only_indexed = rows(Some(IndexedFilter::Flag(true)), None).await;
    assert!(only_indexed.iter().all(|row| row.is_string()), "{only_indexed:?}");
    assert!(only_indexed.iter().any(|row| row == "widget_factory.rs"));

    assert_eq!(rows(Some(IndexedFilter::Word(IndexedWord::Any)), None).await, any);

    let queried = rows(Some(IndexedFilter::Flag(false)), Some("terraform.lock")).await;
    assert_eq!(queried.len(), 1, "{queried:?}");
    assert_eq!(queried[0]["kind"], "lockfile");
}
