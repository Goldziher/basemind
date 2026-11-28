//! The memory tools through the MCP adapter: a refusal keeps the marker the CLI turns into its exit
//! code, and an `individual` note belongs to the workspace's durable owner.

use std::sync::Arc;

use crate::config::Config;
use crate::engine::Workspace;
use crate::engine::types::memory::{MemorySaveParams, MemorySearchToolParams, Visibility};
use crate::store::{Store, VIEW_WORKING};

/// A session over `store` opened the way `serve` opens one.
fn session(root: &std::path::Path, store: Store, read_only: bool) -> super::ServerState {
    let telemetry_dir = store.basemind_dir.clone();
    let shared = Workspace::new(
        store,
        root.to_path_buf(),
        Arc::new(Config::with_defaults()),
        None,
        Arc::new(crate::git_cache::GitCache::open(root, 1, false).expect("open the git cache")),
        None,
        super::ServerOptions {
            read_only,
            ..super::ServerOptions::default()
        }
        .into(),
    );
    // A stand-in model, so no test loads (or downloads) one.
    let fake = |text: &str| Ok(vec![text.len() as f32; 4]);
    let _ = shared
        .embedder
        .set(Arc::new(crate::embeddings::SharedEmbedder::fake("fake", 4, fake)));
    super::ServerState::for_connection(super::ServedWorkspace::new(Arc::new(shared), &telemetry_dir))
}

fn read(key: &str, visibility: Option<Visibility>) -> MemorySearchToolParams {
    MemorySearchToolParams {
        query: None,
        key: Some(key.into()),
        prefix: None,
        tags: None,
        visibility,
        limit: None,
        cursor: None,
    }
}

/// A session that holds no index and is not hosted by the daemon (another process holds the lock and
/// no daemon is up) refuses a memory call naming the fix, marked busy so the CLI exits `3`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_memory_call_without_an_index_is_refused_with_the_fix() {
    crate::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    drop(Store::open(root, VIEW_WORKING).expect("create the workspace"));
    let store = Store::open_read_only_no_index(root, VIEW_WORKING).expect("index-less store");
    let state = session(root, store, true);
    let refused = super::adapter::call(&state, read("k", None), Workspace::memory_search)
        .await
        .expect_err("a session with no index cannot read memory");
    assert_eq!(refused.code, rmcp::model::ErrorCode::INVALID_REQUEST, "{refused:?}");
    assert!(refused.message.contains("`basemind comms start`"), "{refused:?}");
    let exit = crate::cli::run_tool("memory_search", Err(refused)).expect_err("refused");
    assert_eq!(crate::cli::exit::exit_code_for(&exit), crate::cli::exit::BUSY);
}

/// An `individual` note belongs to the workspace's durable owner, not to the session that saved
/// it: a server started again (a new session id) finds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_individual_note_survives_a_server_restart() {
    crate::store::init_isolated_cache();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let first = session(root, Store::open(root, VIEW_WORKING).unwrap(), false);
    let owner = first.shared.memory_owner.clone();
    assert_ne!(owner, "test", "the owner is not the session id");
    let save = MemorySaveParams {
        key: "plan".into(),
        value: "ship on friday".into(),
        tags: None,
        visibility: Some(Visibility::Individual),
    };
    super::adapter::call_unpaged(&first, save, Workspace::memory_save)
        .await
        .unwrap();
    drop(first);

    // An earlier server over the same root releases its index lock a moment after it is dropped.
    let opening = std::time::Instant::now();
    let store = loop {
        match Store::open(root, VIEW_WORKING) {
            Ok(store) => break store,
            Err(_) if opening.elapsed() < std::time::Duration::from_secs(20) => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => panic!("open the store: {error}"),
        }
    };
    let restarted = session(root, store, false);
    assert_eq!(restarted.shared.memory_owner, owner);
    let note = super::adapter::call(
        &restarted,
        read("plan", Some(Visibility::Individual)),
        Workspace::memory_search,
    )
    .await
    .unwrap();
    let note = note.structured_content.expect("structured");
    assert_eq!(
        note["key"], "plan",
        "the note saved before the restart is found: {note}"
    );
}
