//! The one-shot (CLI) server must not preload the whole-corpus in-RAM code map.
//!
//! `MapCache::build` deserializes EVERY indexed file's L1 blob. On a large monorepo that is seconds
//! of flat startup charged to every CLI invocation — including `repo_info`, which never reads the
//! map at all. These tests pin the contract: a one-shot server boots with an empty map, a tool that
//! does not need the corpus never builds it, and a tool that does need it still sees complete data.

use std::sync::Arc;

use rmcp::model::{ArgumentInfo, CallToolResult, CompleteRequestParams, ContentBlock, Reference};
use serde_json::Value;

use super::params::{Lenient, OutlineParams, Parameters, SearchSymbolsParams, StatusParams};
use super::{BasemindServer, ServerState};
use crate::config::ConfigV1;
use crate::git_cache::GitCache;
use crate::scanner::{EmbedMode, ScanSource, scan};
use crate::store::{Store, VIEW_WORKING};

/// Scan a two-file fixture, then hand back a one-shot server over the resulting read-only store —
/// the exact construction `basemind query …` performs per invocation.
fn oneshot_server(root: &std::path::Path) -> BasemindServer {
    // Pin `$BASEMIND_DATA_HOME` before the first store open. Another test in this binary would
    // otherwise flip the process-global mid-run, and the read-only store would look for its index
    // in a different cache root than the scan wrote it to.
    crate::store::init_isolated_cache();
    std::fs::write(root.join("a.rs"), b"pub fn alpha_marker() {}\n").expect("a.rs");
    std::fs::write(root.join("b.rs"), b"pub fn beta_marker() {}\n").expect("b.rs");
    let config = ConfigV1::with_defaults();
    {
        let mut store = Store::open(root, VIEW_WORKING).expect("open rw");
        scan(root, &mut store, &config, ScanSource::WorkingTree, EmbedMode::Inline).expect("scan");
    }
    let store = Store::open_read_only(root, VIEW_WORKING).expect("open ro");
    let git_cache = Arc::new(GitCache::open(&store.basemind_dir, 16, false).expect("git cache"));
    BasemindServer::new_oneshot(store, root.to_path_buf(), Arc::new(config), None, git_cache)
}

/// The JSON payload a tool returns (the first text content block).
fn json_of(result: &CallToolResult) -> Value {
    for content in &result.content {
        if let ContentBlock::Text(text) = content {
            return serde_json::from_str(&text.text).expect("tool payload is JSON");
        }
    }
    panic!("tool returned no text content");
}

fn mapped_files(state: &ServerState) -> usize {
    state.cache.load().by_path.len()
}

/// A one-shot server must boot with an EMPTY code map, and a tool that never touches the map
/// (`status` — a pure metadata query) must not cause it to be built. This is the whole point: the
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

    let result = server.status(Parameters(StatusParams {})).await.expect("status");
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

    let params = OutlineParams {
        path: crate::path::RelPath::from("a.rs"),
        l2: false,
        max_tokens: None,
        format: None,
    };
    let result = server.outline(Parameters(Lenient(params))).await.expect("outline");
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

    // Unbuilt map — exactly what a barrier-less caller would observe.
    let cold = server.complete_argument(&request);
    assert!(
        cold.completion.values.is_empty(),
        "an unbuilt map yields no completions: this is the bug a missing barrier ships"
    );

    // The barrier is what makes the map real. `complete` takes it before delegating here.
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

    let params = SearchSymbolsParams {
        needle: "_marker".to_string(),
        kind: None,
        limit: None,
        max_tokens: None,
        format: None,
        cursor: None,
    };
    let result = server
        .search_symbols(Parameters(Lenient(params)))
        .await
        .expect("search_symbols");
    let payload = json_of(&result);

    let names: Vec<&str> = payload["results"]
        .as_array()
        .expect("results array")
        .iter()
        .filter_map(|hit| hit["name"].as_str())
        .collect();
    assert!(
        names.contains(&"alpha_marker") && names.contains(&"beta_marker"),
        "the on-demand map must cover EVERY indexed file, not just some: got {names:?}"
    );

    assert_eq!(
        mapped_files(&server.state),
        2,
        "the corpus tool built the map on demand"
    );
}
