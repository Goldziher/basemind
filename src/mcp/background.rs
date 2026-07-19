//! Detached background facilities spawned by `serve`: blob GC and the two filesystem watchers.

use std::sync::Arc;

use super::helpers;
use super::{MapCache, ServerState};

/// Run an in-process blob GC once, logging the outcome and swallowing any error.
///
/// Uses the UNLOCKED `store_gc` primitives (`collect_referenced_hashes` + `gc_blobs`)
/// under a `blocking_read()` store guard — NEVER `store_gc::run_gc`, which re-acquires
/// the `.basemind/.lock` flock that `serve` already holds (that would deadlock). The
/// held read guard blocks the only in-process writer (`scan_and_refresh`) for the
/// mark+sweep; cross-process scans are impossible because serve holds the flock.
pub(super) async fn run_background_gc(state: Arc<ServerState>) {
    if state.store.read().await.blobs_shared {
        tracing::debug!("background blob GC skipped: blob cache is shared across git worktrees");
        return;
    }
    let result = tokio::task::spawn_blocking(move || {
        let store = state.store.blocking_read();
        let referenced = crate::store_gc::collect_referenced_hashes(&store.basemind_dir)?;
        crate::store_gc::gc_blobs(&referenced)
    })
    .await;
    match result {
        Ok(Ok(report)) if report.removed > 0 => tracing::info!(
            removed = report.removed,
            bytes_freed = report.bytes_freed,
            "background blob GC reclaimed orphaned blobs"
        ),
        Ok(Ok(_)) => tracing::debug!("background blob GC: nothing to reclaim"),
        Ok(Err(error)) => tracing::warn!(%error, "background blob GC failed"),
        Err(error) => tracing::warn!(%error, "background blob GC task panicked"),
    }
}

/// Boot-time initial index build for an empty index, spawned once from `BasemindServer::new`.
///
/// Two passes so serve becomes queryable fast without pinning the machine on ONNX embedding:
/// 1. A `Deferred` scan writes the code-map + BM25 keyword lane + content-addressed blobs but NO
///    embeddings — this is what clears `initial_scan_active`, so `status` reports the index ready.
/// 2. A detached `Inline` scan then fills the vectors the fast pass skipped, reusing the fast pass'
///    content-addressed caches (only not-yet-embedded content is embedded, bounded by WS4-A's embed
///    pool). GC runs after it settles so the sweep reaps against the final blob set.
pub(super) fn spawn_initial_scan(state: Arc<ServerState>) {
    tracing::info!("empty index on startup; running initial scan in background");
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if state.daemon_writer {
        // A daemon_writer serve holds no write lock: forward the initial full scan to the daemon
        // (the sole writer) and rebuild the read-only map from the index it writes. The fast pass is
        // `Deferred` (code map + keyword lane, no ONNX) so the handshake is never blocked on the
        // embedder; a detached follow-up then forwards an `embed` (Inline) scan so the daemon fills
        // the document + code-chunk vectors the fast pass skipped. Without that follow-up nothing is
        // ever written to LanceDB on this path and `search_documents` stays empty for repo documents
        // (bug #32). This mirrors the non-daemon branch below (Deferred fast pass → detached Inline
        // embed pass → GC), except the embed write is forwarded to the daemon, the sole writer.
        tokio::spawn(async move {
            use std::sync::atomic::Ordering;
            state.initial_scan_active.store(true, Ordering::Relaxed);
            let started = std::time::Instant::now();
            match super::daemon_forward::forward_rescan_and_refresh(&state, None, false, false).await {
                Ok(report) => tracing::info!(
                    scanned = report.scanned,
                    updated = report.updated,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "initial scan complete (forwarded to daemon; embeddings deferred)"
                ),
                Err(error) => tracing::warn!(%error, "initial forwarded scan failed"),
            }
            state
                .initial_scan_ms
                .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            state.initial_scan_active.store(false, Ordering::Relaxed);
            // Detached vector-fill: forward an `embed` (Inline) scan so the daemon fills the vectors
            // the fast pass skipped. No blob GC here — on the daemon-writer model the daemon owns
            // reference-counted GC across all workspaces (a per-workspace serve sweep would be unsafe).
            let embed_state = Arc::clone(&state);
            tokio::spawn(async move {
                let embed_started = std::time::Instant::now();
                tracing::info!("background embedding pass starting (forwarded to daemon)");
                match super::daemon_forward::forward_rescan_and_refresh(&embed_state, None, false, true).await {
                    Ok(report) => tracing::info!(
                        scanned = report.scanned,
                        updated = report.updated,
                        elapsed_ms = embed_started.elapsed().as_millis() as u64,
                        "background embedding pass complete (forwarded to daemon)"
                    ),
                    Err(error) => tracing::warn!(%error, "background forwarded embedding pass failed"),
                }
            });
        });
        return;
    }
    tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        state.initial_scan_active.store(true, Ordering::Relaxed);
        let started = std::time::Instant::now();
        match helpers::scan_and_refresh(Arc::clone(&state), None, crate::scanner::EmbedMode::Deferred).await {
            Ok(report) => tracing::info!(
                scanned = report.stats.scanned,
                updated = report.stats.updated,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "initial background scan complete (code-map + keyword lane; embeddings deferred)"
            ),
            Err(error) => tracing::warn!(%error, "initial background scan failed"),
        }
        state
            .initial_scan_ms
            .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        state.initial_scan_active.store(false, Ordering::Relaxed);
        let embed_state = Arc::clone(&state);
        tokio::spawn(async move {
            let embed_started = std::time::Instant::now();
            tracing::info!("background embedding pass starting");
            match helpers::scan_and_refresh(Arc::clone(&embed_state), None, crate::scanner::EmbedMode::Inline).await {
                Ok(report) => tracing::info!(
                    scanned = report.stats.scanned,
                    updated = report.stats.updated,
                    elapsed_ms = embed_started.elapsed().as_millis() as u64,
                    "background embedding pass complete"
                ),
                Err(error) => tracing::warn!(%error, "background embedding pass failed"),
            }
            run_background_gc(embed_state).await;
        });
    });
}

/// Boot-time in-RAM code-map preload, spawned once from `BasemindServer::new_with_options` when the
/// index is already populated (no initial scan needed) on the background `serve` path.
///
/// `serve` boots with an EMPTY [`MapCache`] placeholder so it can answer the MCP `initialize` /
/// `tools/list` handshake immediately, then this task does the heavy `MapCache::build` (a rayon
/// `par_iter` over every L1/L2 blob) on a blocking thread, publishes the full map via `ArcSwap`, and
/// wakes every tool awaiting [`ServerState::cache_ready`]. Without this, that build ran synchronously
/// before `.serve(transport)` and — under rayon-pool contention from other sessions' scans — could take
/// minutes, blowing the client's startup window so the tools never registered.
pub(super) fn spawn_cache_warm(state: Arc<ServerState>) {
    tracing::info!("warming in-RAM code map in background (handshake already served)");
    tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        let started = std::time::Instant::now();
        let build_state = Arc::clone(&state);
        let built = tokio::task::spawn_blocking(move || {
            let store = build_state.store.blocking_read();
            MapCache::build(&store)
        })
        .await;
        match built {
            Ok(cache) => {
                let files = cache.by_path.len();
                state.cache.store(Arc::new(cache));
                state.cache_generation.fetch_add(1, Ordering::Relaxed);
                state
                    .cache_warm_ms
                    .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                state.cache_warming.store(false, Ordering::Relaxed);
                state.cache_ready.notify_waiters();
                tracing::info!(
                    files,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "in-RAM code map warm complete"
                );
            }
            Err(error) => {
                state.cache_warming.store(false, Ordering::Relaxed);
                state.cache_ready.notify_waiters();
                tracing::error!(%error, "in-RAM code map warm task panicked; serving un-warmed cache");
            }
        }
    });
}

/// Run one debounced batch of changed paths through the writer and report `(scanned, updated,
/// removed)`. A `daemon_writer` serve forwards the batch to the daemon (the sole writer) and
/// rebuilds its read-only map; every other serve scans locally under its own write lock. Bridges
/// the watcher's blocking std thread to the async writer via the captured runtime `Handle`.
fn refresh_batch(
    handle: &tokio::runtime::Handle,
    state: &Arc<ServerState>,
    paths: Vec<std::path::PathBuf>,
) -> Result<(usize, usize, usize), String> {
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if state.daemon_writer {
        // `embed: true` mirrors the non-daemon watcher below (`EmbedMode::Inline`): a document or
        // source file added/edited after boot gets its vectors filled by the daemon, not left
        // chunk-only forever.
        let report = handle
            .block_on(super::daemon_forward::forward_rescan_and_refresh(
                state,
                Some(paths),
                false,
                true,
            ))
            .map_err(|error| error.to_string())?;
        return Ok((report.scanned, report.updated, report.removed));
    }
    let report = handle
        .block_on(helpers::scan_and_refresh(
            Arc::clone(state),
            Some(paths),
            crate::scanner::EmbedMode::Inline,
        ))
        .map_err(|error| error.to_string())?;
    Ok((report.stats.scanned, report.stats.updated, report.stats.removed))
}

/// Active filesystem watcher embedded in `serve` for the working view.
///
/// Unlike [`spawn_view_watcher`] (which is passive — it only reacts to an
/// external process writing `index.msgpack`), this watches the working tree
/// directly and funnels every debounced batch of changed paths into the
/// canonical in-process refresh, [`helpers::scan_and_refresh`]. That re-scans
/// under serve's already-open `Store` (its `RwLock`), so we never open a second
/// `.basemind/.lock` flock — the reason we cannot reuse `watcher::watch`, which
/// owns its own `Store`.
///
/// Threading bridge: `watcher::watch_paths` runs the debouncer on a blocking std
/// thread, but `scan_and_refresh` is async. We capture the current tokio runtime
/// `Handle` at spawn time and `handle.block_on(...)` the refresh inside the
/// callback. `block_on` is safe here because the callback runs on a plain OS
/// thread with no tokio runtime entered (it's `std::thread`, not a worker), so
/// the "cannot block the current thread from within a runtime" guard never trips.
///
/// Lifetime: the thread is detached and runs for the process lifetime, mirroring
/// `spawn_view_watcher`. The `shutdown` oneshot sender is dropped immediately, so
/// `watch_paths`'s `shutdown.try_recv()` returns `Disconnected` only if the loop
/// ever polls it after the sender drops — in practice the loop exits when the
/// process tears down stdio and the debouncer channel closes. A failed
/// incremental refresh is logged and swallowed so a transient scan error never
/// kills the watcher.
pub(super) fn spawn_serve_watcher(state: Arc<ServerState>) {
    let root = state.root.clone();
    let config = Arc::clone(&state.config);
    let handle = tokio::runtime::Handle::current();
    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::Builder::new()
        .name("basemind-mcp-serve-watcher".to_string())
        .spawn(move || {
            let _keep_sender_alive = _shutdown_tx;
            tracing::info!(root = %root.display(), "serve watcher armed (live incremental rescan)");
            let result = crate::watcher::watch_paths(&root, &config, shutdown_rx, |paths, _kind| {
                use std::sync::atomic::Ordering;
                let refresh_state = Arc::clone(&state);
                refresh_state.rescan_active.store(true, Ordering::Relaxed);
                let outcome = refresh_batch(&handle, &refresh_state, paths);
                refresh_state.rescan_active.store(false, Ordering::Relaxed);
                match outcome {
                    Ok((scanned, updated, removed)) => {
                        tracing::debug!(scanned, updated, removed, "serve watcher: incremental rescan complete")
                    }
                    Err(error) => tracing::warn!(
                        %error,
                        "serve watcher: incremental rescan failed (watcher continues)"
                    ),
                }
            });
            if let Err(error) = result {
                tracing::warn!(%error, "serve watcher exited with error");
            }
            tracing::info!("serve watcher: exiting");
        })
        .ok();
}

/// Reopen the working store read-only for a MapCache rebuild. A `daemon_writer` serve opens
/// blobs-only (never the fjall index) so it can't steal the exclusive index lock from its own
/// daemon (the sole writer); every other serve opens the index normally.
fn reopen_read_only(state: &ServerState, view: &str) -> Result<crate::store::Store, crate::store::StoreError> {
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if state.daemon_writer {
        return crate::store::Store::open_read_only_no_index(state.root.as_path(), view);
    }
    crate::store::Store::open_read_only(state.root.as_path(), view)
}

pub(super) fn spawn_view_watcher(state: Arc<ServerState>) {
    let (basemind_dir, view) = {
        let store = match state.store.try_read() {
            Ok(g) => g,
            Err(_) => return,
        };
        (store.basemind_dir.clone(), store.view.clone())
    };
    let view_dir = basemind_dir.join(crate::store::VIEWS_DIR).join(&view);
    let target = view_dir.join(crate::store::INDEX_FILE);

    std::thread::Builder::new()
        .name("basemind-mcp-view-watcher".to_string())
        .spawn(move || {
            use notify_debouncer_full::new_debouncer;
            use std::time::Duration;

            let (tx, rx) = std::sync::mpsc::channel();
            let mut debouncer = match new_debouncer(Duration::from_millis(150), None, tx) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = %e, "view watcher: failed to start debouncer");
                    return;
                }
            };
            if let Err(e) = debouncer.watch(&view_dir, notify::RecursiveMode::NonRecursive) {
                tracing::warn!(error = %e, dir = %view_dir.display(), "view watcher: failed to watch");
                return;
            }
            tracing::info!(target = %target.display(), "view watcher armed");

            while let Ok(result) = rx.recv() {
                let events = match result {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let touches_index = events.iter().any(|de| de.event.paths.iter().any(|p| p == &target));
                if !touches_index {
                    continue;
                }
                let view = state.store.try_read().map(|g| g.view.clone()).unwrap_or_default();
                let new_store = match reopen_read_only(&state, &view) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "view watcher: store reopen failed");
                        continue;
                    }
                };
                // `index.msgpack` being REWRITTEN does not mean any indexed file CHANGED: the daemon
                // rewrites it after every scan, including one that touched nothing. Rebuilding the
                // whole corpus here — re-reading every L1/L2 blob while the old map is still
                // resident — is what made serve's RSS sawtooth. The fingerprint proves the blobs
                // behind the map are identical, so the map we already hold is still exactly right.
                let fingerprint = super::map_fingerprint::index_fingerprint(&new_store);
                if fingerprint == state.cache.load().fingerprint {
                    tracing::debug!("view watcher: index rewritten but unchanged; keeping the current MapCache");
                    continue;
                }
                let new_cache = Arc::new(MapCache::build(&new_store));
                tracing::info!(
                    files = new_cache.by_path.len(),
                    "view watcher: rebuilt MapCache from refreshed index"
                );
                state.cache.store(new_cache);
                state
                    .cache_generation
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            tracing::info!("view watcher: channel closed; exiting");
        })
        .ok();
}
