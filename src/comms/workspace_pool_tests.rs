//! Unit tests for [`WorkspacePool`](super::WorkspacePool). Included from `workspace_pool.rs` via a
//! `#[cfg(test)] #[path = "workspace_pool_tests.rs"] mod tests;` declaration, so `super` resolves to
//! the `workspace_pool` module. Every test seeds an isolated global cache first so writes land in a
//! tempdir, never the real XDG data home.

use std::time::Duration;

use super::*;

/// A temp workspace holding two trivial Rust sources — enough for the scanner to index symbols.
fn workspace_with_sources() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("alpha.rs"), "pub fn alpha() -> u32 { 1 }\n").expect("write alpha");
    std::fs::write(dir.path().join("beta.rs"), "pub fn beta() -> u32 { 2 }\n").expect("write beta");
    dir
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
