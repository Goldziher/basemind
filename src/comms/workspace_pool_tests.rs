//! Unit tests for [`WorkspacePool`](super::WorkspacePool). Included from `workspace_pool.rs` via a
//! `#[cfg(test)] #[path = "workspace_pool_tests.rs"] mod tests;` declaration, so `super` resolves to
//! the `workspace_pool` module. Every test seeds an isolated global cache first so writes land in a
//! tempdir, never the real XDG data home.

use std::time::Duration;

use super::*;

struct UnusedHistoryHost;

impl crate::git_history::remote::HistoryHost for UnusedHistoryHost {
    fn run_history(
        &self,
        _root: PathBuf,
        _op: crate::git_history::proto::GitHistoryOp,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<crate::git_history::proto::GitHistoryReply, crate::git_history::GitHistoryError>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async { unreachable!("plain workspace never opens git history") })
    }
}

/// Make `dir` a project root the workspace-root allow-list accepts (issue #62). `git init` rather
/// than dropping a `basemind.toml` marker: `.git/` is invisible to the scanner, so the exact
/// `scanned` / `updated` counts these fixtures assert stay what they were.
fn git_init(dir: &std::path::Path) {
    let status = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(dir)
        .status()
        .expect("run git init");
    assert!(status.success(), "git init succeeds in {dir:?}");
}

/// A temp workspace holding two trivial Rust sources — enough for the scanner to index symbols.
fn workspace_with_sources() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git_init(dir.path());
    std::fs::write(dir.path().join("alpha.rs"), "pub fn alpha() -> u32 { 1 }\n").expect("write alpha");
    std::fs::write(dir.path().join("beta.rs"), "pub fn beta() -> u32 { 2 }\n").expect("write beta");
    dir
}

#[test]
fn host_rescan_honors_the_pools_shared_drain_token() {
    // Regression (issue #44 follow-up): the in-process host seam must scan under the broker's shared ~keep
    // drain token, NOT a throwaway `ScanCancel::default()`. The broker adopts the pool's token (see ~keep
    // `Broker::with_registry`), so tripping it — as `begin_drain` does on `comms stop` / SIGTERM — ~keep
    // must short-circuit a hosted rescan instead of pinning the runtime through a full, possibly ~keep
    // embedding, scan pass. The old code passed `ScanCancel::default()` here, so a draining daemon ~keep
    // could not interrupt a hosted mid-scan. ~keep
    use crate::mcp::HostBackend;
    store::init_isolated_cache();

    // Baseline: an un-drained pool runs the hosted scan normally.
    let live = WorkspacePool::new(DEFAULT_HOT_CAP);
    let ws = workspace_with_sources();
    let stats = live.host_rescan(ws.path(), None, false, false).expect("hosted scan");
    assert_eq!(stats.scanned, 2, "an un-drained hosted rescan scans both sources");

    // Drained: tripping the pool's shared token short-circuits host_rescan before it scans anything.
    let drained = WorkspacePool::new(DEFAULT_HOT_CAP);
    drained.scan_cancel().cancel();
    let ws2 = workspace_with_sources();
    let stats = drained
        .host_rescan(ws2.path(), None, false, false)
        .expect("drained hosted scan");
    assert_eq!(
        stats.scanned, 0,
        "a drained pool must not run a hosted scan — the shared token wins"
    );
}

#[test]
fn rescan_indexes_sources_and_is_idempotent() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(DEFAULT_HOT_CAP);
    let ws = workspace_with_sources();

    let first = pool
        .rescan(ws.path(), None, false, false, &ScanCancel::default())
        .expect("first scan")
        .0;
    assert_eq!(first.scanned, 2, "both sources considered");
    assert_eq!(first.updated, 2, "both sources newly indexed");

    let second = pool
        .rescan(ws.path(), None, false, false, &ScanCancel::default())
        .expect("second scan")
        .0;
    assert_eq!(second.scanned, 2, "both sources still considered");
    assert_eq!(second.updated, 0, "nothing changed on the second pass");
    assert_eq!(second.skipped_unchanged, 2, "both sources skipped as unchanged");
}

#[test]
fn lru_eviction_keeps_only_the_most_recent_within_the_cap() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(1);
    let ws1 = workspace_with_sources();
    let ws2 = workspace_with_sources();

    pool.rescan(ws1.path(), None, false, false, &ScanCancel::default())
        .expect("scan ws1");
    assert_eq!(pool.len(), 1);

    pool.rescan(ws2.path(), None, false, false, &ScanCancel::default())
        .expect("scan ws2");
    assert_eq!(pool.len(), 1, "cap of 1 holds a single hot workspace");

    let hot = pool.accessed();
    assert_eq!(hot.len(), 1);
    assert_eq!(hot[0].root, ws2.path(), "the most-recently-used workspace survived");
}

#[test]
fn evicted_workspace_lazily_reopens_with_its_committed_index_intact() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(1);
    let ws1 = workspace_with_sources();
    let ws2 = workspace_with_sources();

    pool.rescan(ws1.path(), None, false, false, &ScanCancel::default())
        .expect("scan ws1");
    let hot_files = pool
        .with_workspace(ws1.path(), |store| store.index.files.len())
        .expect("read ws1 while hot");
    assert_eq!(hot_files, 2, "ws1's two sources are indexed while it is hot");

    pool.rescan(ws2.path(), None, false, false, &ScanCancel::default())
        .expect("scan ws2");
    assert_eq!(pool.len(), 1, "cap of 1 holds a single hot workspace");
    assert!(
        pool.accessed().iter().all(|w| w.root != ws1.path()),
        "ws1 must have been evicted from the hot set"
    );

    let recovered = pool
        .with_workspace(ws1.path(), |store| {
            (
                store.index.files.len(),
                store.lookup("alpha.rs").is_some(),
                store.lookup("beta.rs").is_some(),
            )
        })
        .expect("reopen evicted ws1");
    assert_eq!(
        recovered,
        (2, true, true),
        "the reopened workspace recovers its indexed files from disk without a rescan"
    );
}

#[test]
fn accessed_reports_the_hot_set() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(DEFAULT_HOT_CAP);
    let ws = workspace_with_sources();
    pool.rescan(ws.path(), None, false, false, &ScanCancel::default())
        .expect("scan");

    let hot = pool.accessed();
    assert_eq!(hot.len(), 1);
    assert_eq!(hot[0].root, ws.path());
    assert_eq!(hot[0].key, store::workspace_key(ws.path()));
}

/// Regression guard for bug #32: the daemon (the sole fjall writer) must be able to run the
/// [`EmbedMode::Inline`] vector-fill pass, not only the fast [`EmbedMode::Deferred`] code-map pass.
///
/// The `embed` argument threads to the embed mode. The fast pass writes a chunk-only sidecar
/// (`embedding_dim: 0`) and, being unchanged, a second Deferred pass skips it. An `embed == true`
/// pass over the SAME content must re-process the file to fill vectors — exactly the invariant
/// `code_search_smoke::deferred_chunk_only_sidecar_is_reprocessed_by_an_inline_embed_pass` pins at
/// the `scan` layer, here proven through the daemon's pool. Before the fix `rescan` was hard-wired to
/// `Deferred`, so the third pass changed nothing and this assertion failed — the daemon could never
/// embed, leaving `search_code` / `search_documents` empty forever. Embedder-independent: it asserts
/// the file is re-processed, not that a vector was produced (the embedder may be offline in CI).
#[cfg(feature = "code-search")]
#[test]
fn embed_pass_reprocesses_the_chunk_only_sidecar_the_deferred_pass_left() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(DEFAULT_HOT_CAP);
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("basemind.toml"),
        "\"$schema\" = \"v1\"\n[code_search]\nembed = true\n",
    )
    .expect("write config");
    std::fs::write(dir.path().join("lib.rs"), "pub fn embed_marker() -> u32 { 42 }\n").expect("write source");

    let deferred = pool
        .rescan(dir.path(), None, false, false, &ScanCancel::default())
        .expect("deferred scan")
        .0;
    assert!(
        deferred.updated >= 1,
        "the source is newly indexed by the deferred pass"
    );

    let deferred_again = pool
        .rescan(dir.path(), None, false, false, &ScanCancel::default())
        .expect("deferred rescan")
        .0;
    assert_eq!(deferred_again.updated, 0, "a second deferred pass changes nothing");

    let embed = pool
        .rescan(dir.path(), None, false, true, &ScanCancel::default())
        .expect("inline embed scan")
        .0;
    assert!(
        embed.updated >= 1,
        "the embed pass must re-process the chunk-only source to fill vectors (got updated={}, \
         the idempotent deferred pass got {})",
        embed.updated,
        deferred_again.updated
    );
}

/// Bug #32, the document tier: a `Deferred` scan extracts a document but persists no `DocEntry`
/// (`doc_upsert` is `None` under Deferred, see `scanner_file`), so nothing tracks it as embedded and
/// nothing lands in LanceDB. The `embed` (Inline) pass persists the `DocEntry`, the marker that the
/// document was embedded and is reachable via `search_documents`. Before the fix the daemon only ever
/// ran Deferred, so `lookup_doc` stayed `None` forever. Embedder-independent: the `DocEntry` is
/// written purely on the strength of the embed mode, not on a vector being produced.
#[cfg(feature = "documents")]
#[test]
fn embed_pass_indexes_a_document_the_deferred_pass_leaves_untracked() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(DEFAULT_HOT_CAP);
    let dir = tempfile::tempdir().expect("tempdir");
    git_init(dir.path());
    std::fs::write(
        dir.path().join("notes.svg"),
        br#"<svg xmlns="http://www.w3.org/2000/svg"><text>photosynthesis chloroplast glucose oxygen</text></svg>"#,
    )
    .expect("write document");

    let deferred = pool
        .rescan(dir.path(), None, false, false, &ScanCancel::default())
        .expect("deferred scan")
        .0;
    assert!(
        deferred.docs_indexed >= 1,
        "the .svg file must route to the document tier (docs_indexed={})",
        deferred.docs_indexed
    );
    let tracked_after_deferred = pool
        .with_workspace(dir.path(), |store| store.lookup_doc("notes.svg").is_some())
        .expect("read after deferred");
    assert!(
        !tracked_after_deferred,
        "the deferred pass must not persist a document embedding entry"
    );

    pool.rescan(dir.path(), None, false, true, &ScanCancel::default())
        .expect("inline embed scan");
    let tracked_after_inline = pool
        .with_workspace(dir.path(), |store| store.lookup_doc("notes.svg").is_some())
        .expect("read after inline");
    assert!(
        tracked_after_inline,
        "the inline embed pass must index the document so search_documents can reach it"
    );
}

#[test]
fn evict_idle_zero_drops_every_entry() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(DEFAULT_HOT_CAP);
    let ws = workspace_with_sources();
    pool.rescan(ws.path(), None, false, false, &ScanCancel::default())
        .expect("scan");
    assert_eq!(pool.len(), 1);

    let dropped = pool.evict_idle(Duration::ZERO);
    assert_eq!(dropped, 1, "a zero idle window evicts everything");
    assert_eq!(pool.len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_eviction_stops_hosted_watcher_and_releases_read_stack() {
    const RELEASE_TIMEOUT: Duration = Duration::from_secs(2);
    const RELEASE_POLL_INTERVAL: Duration = Duration::from_millis(20);

    store::init_isolated_cache();
    let pool = std::sync::Arc::new(WorkspacePool::new(DEFAULT_HOT_CAP));
    let workspace = workspace_with_sources();
    pool.rescan(workspace.path(), None, false, false, &ScanCancel::default())
        .expect("scan workspace");

    let host: std::sync::Arc<dyn crate::mcp::HostBackend> = pool.clone();
    let history_host: std::sync::Arc<dyn crate::git_history::remote::HistoryHost> =
        std::sync::Arc::new(UnusedHistoryHost);
    let shared = pool
        .get_or_build_serve_state(workspace.path(), host, history_host)
        .await
        .expect("build hosted read stack");
    let weak = std::sync::Arc::downgrade(&shared);
    drop(shared);

    assert_eq!(
        pool.evict_idle(Duration::ZERO),
        1,
        "workspace is removed from the hot pool"
    );
    tokio::time::timeout(RELEASE_TIMEOUT, async {
        while weak.upgrade().is_some() {
            tokio::time::sleep(RELEASE_POLL_INTERVAL).await;
        }
    })
    .await
    .expect("eviction must stop the hosted watcher and release its read stack");
}

/// Full rescans that pile up behind an identical in-flight full scan coalesce: both requests
/// capture the full-scan generation before blocking on the store lock, the winner walks the tree,
/// and the loser is served the winner's stats. A NON-coalesced second pass would have reported
/// `updated == 0, skipped_unchanged == 2` — both reporting `updated == 2` proves one walk ran.
/// This is the issue-#44 pile-up: N sessions each requesting a full rescan of the same monorepo.
#[test]
fn queued_identical_full_rescans_coalesce() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(DEFAULT_HOT_CAP);
    let ws = workspace_with_sources();

    let entry = pool.get_or_open(ws.path()).expect("open entry");
    let guard = entry.store.lock().unwrap_or_else(PoisonError::into_inner);

    std::thread::scope(|scope| {
        let a = scope.spawn(|| pool.rescan(ws.path(), None, true, false, &ScanCancel::default()));
        let b = scope.spawn(|| pool.rescan(ws.path(), None, true, false, &ScanCancel::default()));
        std::thread::sleep(Duration::from_millis(200));
        drop(guard);

        let (stats_a, cancelled_a) = a.join().expect("thread a").expect("rescan a");
        let (stats_b, cancelled_b) = b.join().expect("thread b").expect("rescan b");
        assert!(!cancelled_a && !cancelled_b);
        assert_eq!(stats_a.updated, 2, "one request performed the real walk");
        assert_eq!(
            stats_b.updated, 2,
            "the queued request was served the winner's stats, not a redundant re-walk"
        );
        assert_eq!(stats_a.skipped_unchanged, 0);
        assert_eq!(stats_b.skipped_unchanged, 0);
    });
}

/// Sequential full rescans must NOT coalesce: the second request captures the already-advanced
/// generation, so it walks the tree itself and correctly reports everything unchanged.
#[test]
fn sequential_full_rescans_do_not_coalesce() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(DEFAULT_HOT_CAP);
    let ws = workspace_with_sources();

    let first = pool
        .rescan(ws.path(), None, true, false, &ScanCancel::default())
        .expect("first full scan")
        .0;
    assert_eq!(first.updated, 2);

    let second = pool
        .rescan(ws.path(), None, true, false, &ScanCancel::default())
        .expect("second full scan")
        .0;
    assert_eq!(second.updated, 0, "a sequential rescan really re-walks");
    assert_eq!(second.skipped_unchanged, 2);
}

/// A request whose drain token is already tripped must fail fast as a cancelled pass — never
/// start its own tree walk. This covers the request that queued on the store mutex behind a
/// pass the drain cancelled: the cancelled pass never advances `full_scan_gen`, so generation
/// coalescing cannot catch it, and only this re-check stops a re-walk storm.
#[test]
fn a_tripped_cancel_token_fails_fast_without_walking() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(DEFAULT_HOT_CAP);
    let ws = workspace_with_sources();

    let tripped = ScanCancel::default();
    tripped.cancel();
    let (stats, cancelled) = pool
        .rescan(ws.path(), None, true, false, &tripped)
        .expect("a tripped token is a cancelled pass, not an error");
    assert!(cancelled, "the pass reports itself cancelled");
    assert_eq!(stats.scanned, 0, "no files were walked");

    let (stats, cancelled) = pool
        .rescan(ws.path(), None, true, false, &ScanCancel::default())
        .expect("a fresh token scans normally");
    assert!(!cancelled);
    assert_eq!(stats.scanned, 2, "the cancelled pass left no coalescing residue");
}

#[test]
fn active_connection_guard_blocks_lru_eviction() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(1);
    let ws1 = workspace_with_sources();
    let ws2 = workspace_with_sources();

    pool.rescan(ws1.path(), None, false, false, &ScanCancel::default())
        .expect("scan ws1");
    let guard = pool.begin_conn(ws1.path()).expect("begin conn ws1");

    // ~keep ws2 opens past the cap, but ws1 has a live connection, so it must NOT be evicted.
    pool.rescan(ws2.path(), None, false, false, &ScanCancel::default())
        .expect("scan ws2");
    assert_eq!(pool.len(), 2, "an active connection holds ws1 hot past the cap");
    assert!(
        pool.accessed().iter().any(|w| w.root == ws1.path()),
        "ws1 stays hot while a connection is served against it"
    );

    // ~keep Once the connection drains, ws1 is evictable again.
    drop(guard);
    let ws3 = workspace_with_sources();
    pool.rescan(ws3.path(), None, false, false, &ScanCancel::default())
        .expect("scan ws3");
    assert!(
        pool.accessed().iter().all(|w| w.root != ws1.path()),
        "ws1 is evictable once no connection holds it"
    );
}

#[test]
fn active_connection_guard_blocks_idle_sweep() {
    store::init_isolated_cache();
    let pool = WorkspacePool::new(4);
    let ws = workspace_with_sources();

    pool.rescan(ws.path(), None, false, false, &ScanCancel::default())
        .expect("scan");
    let guard = pool.begin_conn(ws.path()).expect("begin conn");

    assert_eq!(
        pool.evict_idle(Duration::ZERO),
        0,
        "an active connection is never idle-swept, even at a zero idle threshold"
    );
    assert_eq!(pool.len(), 1);

    drop(guard);
    assert_eq!(
        pool.evict_idle(Duration::ZERO),
        1,
        "the workspace is idle-swept once its last connection drains"
    );
    assert_eq!(pool.len(), 0);
}
