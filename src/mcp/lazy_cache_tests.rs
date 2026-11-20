//! The one-shot (CLI) server must not preload the whole-corpus in-RAM code map.
//!
//! `MapCache::build` deserializes EVERY indexed file's L1 blob. On a large monorepo that is seconds
//! of flat startup charged to every CLI invocation — including `repo_info`, which never reads the
//! map at all. These tests pin the contract: a one-shot server boots with an empty map, a tool that
//! does not need the corpus never builds it, and a tool that does need it still sees complete data.

use std::sync::Arc;

use rmcp::model::{ArgumentInfo, CallToolResult, CompleteRequestParams, ContentBlock, Reference};
use serde_json::Value;

use super::params::{CodeOutlineParams, CodeSymbolsParams, IndexStatusParams};
use super::{BasemindServer, ServerState};
use crate::config::ConfigV1;
use crate::git_cache::GitCache;
use crate::scanner::{EmbedMode, ScanSource, scan};
use crate::store::{Store, VIEW_WORKING};

/// Scan a two-file fixture, then hand back a one-shot server over the resulting read-only store —
/// the exact construction `basemind query …` performs per invocation.
fn oneshot_server(root: &std::path::Path) -> BasemindServer {
    crate::store::init_isolated_cache();
    std::fs::write(root.join("a.rs"), b"pub fn alpha_marker() {}\n").expect("a.rs");
    std::fs::write(root.join("b.rs"), b"pub fn beta_marker() {}\n").expect("b.rs");
    let config = ConfigV1::with_defaults();
    {
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
    }
    let store = Store::open_read_only(root, VIEW_WORKING).expect("open ro");
    let git_cache = Arc::new(GitCache::open(&store.basemind_dir, 16, false).expect("git cache"));
    BasemindServer::new_oneshot(store, root.to_path_buf(), Arc::new(config), None, git_cache)
}

/// The JSON payload a tool returns (the first text content block).
fn json_of(result: &CallToolResult) -> Value {
    for content in &result.content {
        if let ContentBlock::Text(text) = content {
            return crate::wire::decode(&text.text).expect("tool payload reads back");
        }
    }
    panic!("tool returned no text content");
}

fn mapped_files(state: &ServerState) -> usize {
    state.shared.cache.load().len()
}

/// A one-shot server must boot with an EMPTY code map, and a tool that never touches the map
/// (`index_status`, a pure metadata query) must not cause it to be built. This is the whole point: the
/// flat whole-corpus `MapCache::build` cost must not be charged to a query that never reads it.
#[tokio::test]
async fn one_shot_server_does_not_preload_the_code_map() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let server = oneshot_server(tmp.path());

    assert_eq!(
        mapped_files(&server.state),
        0,
        "one-shot server must boot with an unbuilt code map, not a preloaded whole-corpus mirror"
    );

    let result = server
        .index_status_call(IndexStatusParams {})
        .await
        .expect("index_status");
    let payload = json_of(&result);
    assert_eq!(
        payload["file_count"].as_u64(),
        Some(2),
        "status still reports the full indexed corpus — it reads the store, not the in-RAM map"
    );

    assert_eq!(
        mapped_files(&server.state),
        0,
        "status never reads the code map, so it must not trigger a whole-corpus build"
    );
}

/// `outline` is PATH-KEYED: it needs one file's L1, and it already falls back to a single blob read
/// when the in-RAM map misses. So it must answer from the store rather than force a whole-corpus
/// build — otherwise the commonest navigation call pays the full corpus cost to read one file.
#[tokio::test]
async fn outline_answers_from_the_store_without_building_the_corpus_map() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let server = oneshot_server(tmp.path());
    assert_eq!(mapped_files(&server.state), 0, "starts unbuilt");

    let params = CodeOutlineParams {
        path: crate::path::RelPath::from("a.rs"),
        limit: None,
        cursor: None,
        max_tokens: None,
    };
    let result = server.code_outline_call(params).await.expect("outline");
    let payload = json_of(&result);

    let names: Vec<&str> = payload["symbols"]
        .as_array()
        .expect("symbols array")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert_eq!(names, vec!["alpha_marker"], "outline returns the file's real symbols");

    assert_eq!(
        mapped_files(&server.state),
        0,
        "outline reads ONE blob; it must not deserialize every L1 in the corpus to do it"
    );
}

/// `completion/complete` scans the in-RAM map, so it is only correct BEHIND the barrier.
///
/// This pins the hazard directly: on an unbuilt map `complete_argument` returns nothing, so any
/// caller that reads the cache without `await_cache_ready` serves zero candidates — a race against
/// the background warm on `serve`, and a permanent empty under `lazy_cache`, where the barrier is
/// what builds the map. `BasemindServer::complete` therefore takes the barrier first.
#[tokio::test]
async fn completion_is_empty_until_the_barrier_builds_the_map() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let server = oneshot_server(tmp.path());

    let request = CompleteRequestParams::new(
        Reference::for_prompt("trace-symbol"),
        ArgumentInfo::new("symbol", "alpha"),
    );

    let cold = server.complete_argument(&request);
    assert!(
        cold.completion.values.is_empty(),
        "an unbuilt map yields no completions: this is the bug a missing barrier ships"
    );

    server.state.await_cache_ready().await;
    let warm = server.complete_argument(&request);
    assert!(
        warm.completion.values.iter().any(|v| v == "alpha_marker"),
        "after the barrier, completion sees the corpus: got {:?}",
        warm.completion.values
    );
}

/// Laziness must not cost correctness: a tool that DOES need the whole-corpus map
/// (`search_symbols` scans every file's symbols in RAM) must still see complete data — the map is
/// built on demand at the `await_cache_ready` barrier before the scan runs.
#[tokio::test]
async fn corpus_tool_builds_the_map_on_demand_and_sees_every_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let server = oneshot_server(tmp.path());
    assert_eq!(mapped_files(&server.state), 0, "starts unbuilt");

    let params = CodeSymbolsParams {
        name: "_marker".to_string(),
        kind: None,
        path: None,
        language: None,
        fuzzy: None,
        limit: None,
        cursor: None,
        max_tokens: None,
        changed: None,
    };
    let result = server.code_symbols_call(params).await.expect("code_symbols");
    let payload = json_of(&result);

    let names: Vec<&str> = payload["results"]
        .as_array()
        .expect("results")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    // A row is `"<path>:<line>: <signature>"`: the name is read from the signature, not a path.
    let has = |name: &str| {
        names.iter().any(|row| {
            row.split_once(": ")
                .is_some_and(|(_, signature)| signature.contains(name))
        })
    };
    assert!(
        has("alpha_marker") && has("beta_marker"),
        "the on-demand map must cover EVERY indexed file, not just some: got {names:?}"
    );

    assert_eq!(
        mapped_files(&server.state),
        2,
        "the corpus tool built the map on demand"
    );
}

/// A store whose index a scan must rebuild says so in `index_status`: a flag, and a note naming the
/// command. Without it every symbol query comes back empty with nothing to say why.
#[tokio::test]
async fn index_status_says_when_a_rescan_is_owed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    crate::store::init_isolated_cache();
    std::fs::write(tmp.path().join("a.rs"), b"pub fn alpha_marker() {}\n").expect("a.rs");
    {
        let mut store = Store::open(tmp.path(), VIEW_WORKING).expect("open rw");
        let config = ConfigV1::with_defaults();
        scan(
            tmp.path(),
            &mut store,
            &config,
            ScanSource::WorkingTree,
            EmbedMode::Inline,
            crate::retrieval_wiring::backend(),
        )
        .expect("scan");
        // The state a writer leaves when it emptied the file map for a rebuild and died before it.
        store.index.rebuild_pending = true;
        store.flush().expect("flush");
    }
    let store = Store::open_read_only(tmp.path(), VIEW_WORKING).expect("open ro");
    assert!(store.rescan_owed());
    let git_cache = Arc::new(GitCache::open(&store.basemind_dir, 16, false).expect("git cache"));
    let config = Arc::new(ConfigV1::with_defaults());
    let server = BasemindServer::new_oneshot(store, tmp.path().to_path_buf(), config, None, git_cache);

    let result = server
        .index_status_call(IndexStatusParams {})
        .await
        .expect("index_status");
    let payload = json_of(&result);

    assert_eq!(payload["rescan_needed"], true, "{payload}");
    let note = payload["note"].as_str().expect("a note says what to run");
    assert!(note.contains("basemind scan"), "{note}");
}

/// `index_status` carries the watcher's watch counts, and names the sysctl once the inotify watch
/// limit left directories unwatched. A workspace whose watcher published nothing has no block.
#[tokio::test]
async fn index_status_reports_watch_limit_exhaustion() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let server = oneshot_server(tmp.path());
    let before = json_of(
        &server
            .index_status_call(IndexStatusParams {})
            .await
            .expect("index_status"),
    );
    assert!(before.get("watches").is_none(), "{before}");

    server.state.shared.watch_health.record(5, 2);
    let payload = json_of(
        &server
            .index_status_call(IndexStatusParams {})
            .await
            .expect("index_status"),
    );
    let watches = &payload["watches"];
    assert_eq!(watches["held"], 5, "{payload}");
    assert_eq!(watches["unwatched"], 2, "{payload}");
    let note = watches["note"].as_str().expect("an exhausted limit has a note");
    assert!(note.contains("2 directories are not watched"), "{note}");
    assert!(note.contains("fs.inotify.max_user_watches"), "{note}");
}

/// Publishing the map of the index the server opened changes no outline: a cursor cut before the
/// lazy build (on `serve`, the boot-time warm) must still page after it. Both used to bump the one
/// generation every cursor is stamped with, so the next page read as a changed index.
#[tokio::test]
async fn an_outline_cursor_survives_the_map_being_built() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("c.rs"), b"pub fn one() {}\npub fn two() {}\n").expect("c.rs");
    let server = oneshot_server(tmp.path());
    let outline = |cursor| CodeOutlineParams {
        path: crate::path::RelPath::from("c.rs"),
        limit: Some(1),
        cursor,
        max_tokens: None,
    };
    let names = |payload: &Value| -> Vec<String> {
        payload["symbols"]
            .as_array()
            .expect("symbols array")
            .iter()
            .filter_map(|symbol| symbol["name"].as_str().map(str::to_owned))
            .collect()
    };

    let first = json_of(&server.code_outline_call(outline(None)).await.expect("first page"));
    let cursor = first["next_cursor"].as_str().expect("a second page").to_owned();

    server.state.await_cache_ready().await;
    assert_eq!(mapped_files(&server.state), 3, "the barrier built the map");

    let second = server
        .code_outline_call(outline(Some(super::cursor::Cursor(cursor))))
        .await
        .expect("the cursor still pages after the map was built");
    let second = json_of(&second);
    assert_eq!(names(&first).len(), 1, "{first}");
    assert_eq!(names(&second).len(), 1, "{second}");
    assert_ne!(names(&first), names(&second), "the cursor advances: {second}");
}
