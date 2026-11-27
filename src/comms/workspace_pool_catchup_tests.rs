//! The catch-up pass a hosted read stack runs when it is built over a populated index: edits made
//! while no watcher ran (the stack shed, the daemon gone, a `git checkout` in between) are indexed
//! without anyone calling `index_rescan`. Included from `workspace_pool_tests.rs`, whose fixtures
//! `super` resolves to.

use std::sync::atomic::Ordering;

use super::*;

/// `code_symbols {name}` against `shared`, as a client of the hosted stack reads it.
async fn symbols(shared: &crate::mcp::ServedWorkspace, name: &str) -> serde_json::Value {
    let server = crate::mcp::BasemindServer::from_shared(
        shared.clone(),
        std::sync::Arc::new(crate::mcp::admission::HeavyAdmission::default()),
        None,
    );
    let result = server
        .code_symbols_call(crate::mcp::params::CodeSymbolsParams {
            name: name.to_string(),
            kind: None,
            path: None,
            language: None,
            roles: None,
            fuzzy: None,
            limit: None,
            cursor: None,
            max_tokens: None,
            changed: None,
        })
        .await
        .expect("code_symbols answers");
    result.structured_content.expect("structured content")
}

/// A stack the warm cap or idle clock shed runs no watcher until it is rebuilt. A file added in
/// between is found by the rebuilt stack without a rescan call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rebuilt_read_stack_catches_up_on_a_file_added_while_it_was_shed() {
    store::init_isolated_cache();
    let pool = std::sync::Arc::new(WorkspacePool::new(DEFAULT_HOT_CAP));
    let workspace = workspace_with_sources();
    pool.rescan(workspace.path(), None, false, false, &ScanCancel::default())
        .expect("scan workspace");
    drop(hosted_stack(&pool, workspace.path()).await);
    assert_eq!(pool.evict_idle_read_stacks(Duration::ZERO), 1, "the stack is shed");

    std::fs::write(
        workspace.path().join("gamma.rs"),
        "pub fn gamma_added_while_shed() -> u32 { 3 }\n",
    )
    .expect("write gamma");
    let rebuilt = hosted_stack(&pool, workspace.path()).await;
    let found = symbols(&rebuilt, "gamma_added_while_shed").await;
    assert_eq!(
        found["results"],
        serde_json::json!(["gamma.rs:1: pub fn gamma_added_while_shed() -> u32"]),
        "{found}"
    );
}

/// An existing file edited while its stack was shed is re-extracted by the rebuild's catch-up: its
/// stat no longer matches the file map, so the rebuilt stack serves the new symbol and not the old.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rebuilt_read_stack_catches_up_on_a_file_edited_while_it_was_shed() {
    store::init_isolated_cache();
    let pool = std::sync::Arc::new(WorkspacePool::new(DEFAULT_HOT_CAP));
    let workspace = workspace_with_sources();
    pool.rescan(workspace.path(), None, false, false, &ScanCancel::default())
        .expect("scan workspace");
    drop(hosted_stack(&pool, workspace.path()).await);
    assert_eq!(pool.evict_idle_read_stacks(Duration::ZERO), 1, "the stack is shed");

    std::fs::write(
        workspace.path().join("alpha.rs"),
        "pub fn renamed_while_shed() -> u32 { 10 }\n",
    )
    .expect("edit alpha");
    let rebuilt = hosted_stack(&pool, workspace.path()).await;
    let renamed = symbols(&rebuilt, "renamed_while_shed").await;
    assert_eq!(
        renamed["results"],
        serde_json::json!(["alpha.rs:1: pub fn renamed_while_shed() -> u32"]),
        "{renamed}"
    );
    let alpha = symbols(&rebuilt, "alpha").await;
    assert_eq!(
        alpha["results"],
        serde_json::json!([]),
        "the edit replaced the old symbol: {alpha}"
    );
}

/// A workspace the daemon dropped altogether (the idle sweep, or a daemon exit) reopens over its
/// committed index, which a file added meanwhile is missing from.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reopened_workspace_catches_up_on_a_file_added_while_it_was_closed() {
    store::init_isolated_cache();
    let pool = std::sync::Arc::new(WorkspacePool::new(DEFAULT_HOT_CAP));
    let workspace = workspace_with_sources();
    pool.rescan(workspace.path(), None, false, false, &ScanCancel::default())
        .expect("scan workspace");
    drop(hosted_stack(&pool, workspace.path()).await);
    assert_eq!(pool.evict_idle(Duration::ZERO), 1, "the workspace is closed");

    std::fs::remove_file(workspace.path().join("beta.rs")).expect("remove beta");
    std::fs::write(
        workspace.path().join("gamma.rs"),
        "pub fn gamma_added_while_closed() -> u32 { 3 }\n",
    )
    .expect("write gamma");
    let reopened = hosted_stack(&pool, workspace.path()).await;
    // A page without a cursor lists every match and writes no `total`: the rows are the count.
    assert_eq!(
        symbols(&reopened, "gamma_added_while_closed").await["results"],
        serde_json::json!(["gamma.rs:1: pub fn gamma_added_while_closed() -> u32"])
    );
    let beta = symbols(&reopened, "beta").await;
    assert_eq!(
        beta["results"],
        serde_json::json!([]),
        "the deleted file is pruned: {beta}"
    );
}

/// The catch-up is the stat-and-hash pass of `index_rescan {}`: over a tree nothing changed in, it
/// re-extracts no file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_catch_up_over_an_unchanged_tree_extracts_nothing() {
    store::init_isolated_cache();
    let pool = std::sync::Arc::new(WorkspacePool::new(DEFAULT_HOT_CAP));
    let workspace = workspace_with_sources();
    pool.rescan(workspace.path(), None, false, false, &ScanCancel::default())
        .expect("scan workspace");
    let entry = pool.get_or_open(workspace.path()).expect("hot entry");
    let before = entry.full_scan_gen.load(Ordering::Acquire);

    let shared = hosted_stack(&pool, workspace.path()).await;
    assert_eq!(
        shared.workspace.index_generation.load(Ordering::Relaxed),
        1,
        "an unchanged pass publishes nothing, so no session's cursor is cut short"
    );
    assert_eq!(
        entry.full_scan_gen.load(Ordering::Acquire),
        before + 1,
        "building the stack ran one whole-tree pass"
    );
    let last = *entry.last_full.lock().unwrap_or_else(PoisonError::into_inner);
    let (_, _, stats) = last.expect("the pass recorded its stats");
    assert_eq!(
        (stats.scanned, stats.updated, stats.removed, stats.skipped_unchanged),
        (2, 0, 0, 2),
        "every file was judged unchanged and none re-extracted"
    );
}

/// Whether `pool`'s hosted stack for `root` is still built.
fn stack_is_built(pool: &WorkspacePool, root: &std::path::Path) -> bool {
    let entry = pool.get_or_open(root).expect("hot entry");
    entry.serve_state.try_lock().expect("the stack slot is free").is_some()
}

/// A config file appearing, or an edit that changes nothing a scan or the watcher reads, keeps the
/// built stack: a rebuilt one would run a whole-tree catch-up pass and report `rescanning` for nothing.
/// An edit that changes what a scan reads, or the watcher's debounce, sheds the stack so its rebuild
/// catches the index up under the new config.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_config_edit_sheds_the_read_stack_only_when_a_scan_reads_the_change() {
    store::init_isolated_cache();
    let pool = std::sync::Arc::new(WorkspacePool::new(DEFAULT_HOT_CAP));
    let workspace = workspace_with_sources();
    pool.rescan(workspace.path(), None, false, false, &ScanCancel::default())
        .expect("scan workspace");
    drop(hosted_stack(&pool, workspace.path()).await);
    let toml = workspace.path().join("basemind.toml");

    std::fs::write(&toml, "\"$schema\" = \"v1\"\n").expect("create the config file");
    assert!(
        stack_is_built(&pool, workspace.path()),
        "a new file holding the defaults changes nothing a scan reads"
    );

    std::fs::write(&toml, "\"$schema\" = \"v1\"\n# a note\n").expect("edit a comment");
    assert!(
        stack_is_built(&pool, workspace.path()),
        "a comment is neither a scan input nor a watcher setting"
    );

    std::fs::write(&toml, "\"$schema\" = \"v1\"\n[scan]\nexclude = [\"generated\"]\n").expect("edit [scan]");
    assert!(
        !stack_is_built(&pool, workspace.path()),
        "an exclude change is a scan input: the stack is rebuilt and catches up"
    );
}

/// A scan-affecting edit lands while a session holds the stack, so the reload keeps it; once the
/// session leaves, the next connection must not reuse the stack built under the superseded config,
/// and a debounce edit reaches the watcher the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_after_a_config_edit_never_reuses_a_stack_built_under_the_old_config() {
    store::init_isolated_cache();
    let pool = std::sync::Arc::new(WorkspacePool::new(DEFAULT_HOT_CAP));
    let workspace = workspace_with_sources();
    pool.rescan(workspace.path(), None, false, false, &ScanCancel::default())
        .expect("scan workspace");
    let toml = workspace.path().join("basemind.toml");

    for edit in [
        "\"$schema\" = \"v1\"\n[scan]\nexclude = [\"generated\"]\n",
        "\"$schema\" = \"v1\"\n[watch]\ndebounce_ms = 999\n",
    ] {
        let (_stack, conn) = pool
            .serve_conn(workspace.path(), |_| None)
            .await
            .expect("first session");
        std::fs::write(&toml, edit).expect("edit the config during the session");
        pool.get_or_open(workspace.path()).expect("reload");
        assert!(
            stack_is_built(&pool, workspace.path()),
            "a live session keeps its stack"
        );
        drop(conn);
        assert!(
            stack_is_built(&pool, workspace.path()),
            "the idle stack waits for the next connection"
        );

        let (_next, conn) = pool
            .serve_conn(workspace.path(), |_| None)
            .await
            .expect("second session");
        let entry = pool.get_or_open(workspace.path()).expect("entry");
        assert_eq!(
            entry.serve_digest.lock().expect("digest").as_deref(),
            Some(config::rules::read_stack_digest(&entry.config()).as_str()),
            "the rebuilt stack records the current config"
        );
        drop(conn);
    }
}
