//! `code_refs` through the MCP adapter: a refusal keeps the marker the CLI turns into its exit code.

use crate::config::ConfigV1;
use crate::scanner::{ScanSource, scan};
use crate::store::{Store, VIEW_WORKING};

/// Scan `root` into its writer store.
fn scan_fixture(root: &std::path::Path) -> Store {
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    scan(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Inline,
    )
    .expect("scan");
    store
}

/// A session that holds no index (another process holds the lock, no daemon serves this one: a CLI
/// command beside a writer `serve`) refuses every `code_refs` relation that scans it, naming the fix
/// and marked busy so the CLI exits `3`, instead of answering from a partial in-RAM projection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_without_an_index_is_refused_with_the_fix() {
    use std::sync::Arc;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").expect("a.rs");
    std::fs::write(root.join("b.rs"), b"fn beta() { alpha(); }\n").expect("b.rs");
    drop(scan_fixture(root));
    let store = Store::open_read_only_no_index(root, VIEW_WORKING).expect("index-less store");
    let telemetry_dir = store.basemind_dir.clone();
    let shared = crate::mcp::Workspace::new(
        store,
        root.to_path_buf(),
        Arc::new(ConfigV1::with_defaults()),
        None,
        Arc::new(crate::git_cache::GitCache::open(root, 1, false).expect("git cache")),
        None,
        crate::mcp::ServerOptions {
            read_only: true,
            ..crate::mcp::ServerOptions::default()
        }
        .into(),
    );
    let state = crate::mcp::ServerState::for_connection(
        crate::mcp::ServedWorkspace::new(Arc::new(shared), &telemetry_dir),
        "test".into(),
    );
    let refs = |relation: Option<&str>, path: Option<&str>| crate::mcp::params::CodeRefsParams {
        name: "alpha".to_string(),
        relation: relation.map(str::to_string),
        path: path.map(crate::path::RelPath::from),
        line: None,
        column: None,
        match_mode: None,
        limit: None,
        cursor: None,
        max_tokens: None,
    };
    for params in [
        refs(None, None),
        refs(None, Some("a.rs")),
        refs(Some("implementations"), None),
    ] {
        let refused = super::adapter::call(&state, params, crate::engine::Workspace::code_refs)
            .await
            .expect_err("a session with no index cannot scan references");
        assert_eq!(refused.code, rmcp::model::ErrorCode::INVALID_REQUEST, "{refused:?}");
        assert!(refused.message.contains("holds this workspace's index"), "{refused:?}");
        #[cfg(all(feature = "comms", any(unix, windows)))]
        assert!(refused.message.contains("`basemind comms start`"), "{refused:?}");
        let exit = crate::cli::run_tool("code_refs", Err(refused)).expect_err("refused");
        assert_eq!(crate::cli::exit::exit_code_for(&exit), crate::cli::exit::BUSY);
    }
}
