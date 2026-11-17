//! Memory calls that are refused: no index and no daemon host, or a vector store that will not open.

use super::*;

/// A session that holds no index and is not hosted by the daemon (another process holds the lock and
/// no daemon is up) refuses a memory call naming the fix, marked busy so the CLI exits `3`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_memory_call_without_an_index_is_refused_with_the_fix() {
    use crate::store::{Store, VIEW_WORKING};

    crate::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    drop(Store::open(root, VIEW_WORKING).expect("create the workspace"));
    let store = Store::open_read_only_no_index(root, VIEW_WORKING).expect("index-less store");
    let telemetry_dir = store.basemind_dir.clone();
    let shared = crate::mcp::Workspace::new(
        store,
        root.to_path_buf(),
        Arc::new(crate::config::Config::with_defaults()),
        None,
        Arc::new(crate::git_cache::GitCache::open(root, 1, false).unwrap()),
        None,
        crate::mcp::ServerOptions {
            read_only: true,
            ..crate::mcp::ServerOptions::default()
        }
        .into(),
    );
    let state = ServerState::for_connection(
        crate::mcp::ServedWorkspace::new(Arc::new(shared), &telemetry_dir),
        "test".into(),
    );
    let params = crate::mcp::types_memory::MemoryGetParams {
        key: "k".into(),
        visibility: Default::default(),
    };
    let refused = crate::mcp::memory::run_memory_get(&state, params)
        .await
        .expect_err("a session with no index cannot read memory");
    assert_eq!(refused.code, rmcp::model::ErrorCode::INVALID_REQUEST, "{refused:?}");
    assert!(refused.message.contains("`basemind comms start`"), "{refused:?}");
    let exit = crate::cli::run_tool("memory_get", Err(refused)).expect_err("refused");
    assert_eq!(crate::cli::exit::exit_code_for(&exit), crate::cli::exit::BUSY);
}

/// A store that would not open (here a file in its place; in the field another open holding its
/// lock past the wait) answers the calls after it at once, as unavailable, instead of each one
/// trying again and waiting out the same holder.
#[tokio::test]
async fn a_store_that_would_not_open_is_unavailable_at_once_to_the_next_call() {
    let root = tempfile::tempdir().unwrap();
    let state = server(root.path(), "fake");
    save(&state, "k", "a note").await;
    let dir = memory_lance_dir(root.path(), "fake", DIM);
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(&dir, b"not a directory").unwrap();
    let params = || MemorySearchParams {
        query: "a note".into(),
        limit: Some(5),
        tag: None,
        visibility: Visibility::Group,
    };
    let first = run_memory_search(&state, params()).await.unwrap_err();
    assert!(
        first.message.starts_with("`memory_search`: vector store unavailable"),
        "{}",
        first.message
    );
    assert_eq!(
        first.code,
        rmcp::model::ErrorCode::INVALID_REQUEST,
        "a refusal, not an internal error"
    );

    std::fs::remove_file(&dir).unwrap();
    let second = run_memory_search(&state, params()).await.unwrap_err();
    assert!(second.message.contains("unavailable"), "{}", second.message);
    assert!(!dir.exists(), "the store was not opened again");
}
