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
    let result = tokio::task::spawn_blocking(move || {
        let store = state.store.blocking_read();
        let referenced = crate::store_gc::collect_referenced_hashes(&store.basemind_dir)?;
        crate::store_gc::gc_blobs(&store.basemind_dir, &referenced)
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
    // Keep the sender alive for the process lifetime by leaking it into the
    // detached closure-free slot: we never signal shutdown explicitly (the
    // process exit tears the watcher down), so hold the receiver and drop the
    // sender at the end of `serve`'s life via the thread owning it.
    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::Builder::new()
        .name("basemind-mcp-serve-watcher".to_string())
        .spawn(move || {
            // Hold the sender for the whole watcher lifetime so the receiver never
            // sees `Disconnected` early; the watcher exits when the debouncer
            // channel closes at process teardown.
            let _keep_sender_alive = _shutdown_tx;
            tracing::info!(root = %root.display(), "serve watcher armed (live incremental rescan)");
            let result =
                crate::watcher::watch_paths(&root, &config, shutdown_rx, |paths, _kind| {
                    let refresh_state = Arc::clone(&state);
                    // Bridge the blocking watcher thread into the async refresh.
                    match handle.block_on(helpers::scan_and_refresh(refresh_state, Some(paths))) {
                        Ok(report) => tracing::debug!(
                            scanned = report.stats.scanned,
                            updated = report.stats.updated,
                            removed = report.stats.removed,
                            "serve watcher: incremental rescan complete"
                        ),
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
                let touches_index = events
                    .iter()
                    .any(|de| de.event.paths.iter().any(|p| p == &target));
                if !touches_index {
                    continue;
                }
                let new_store = match crate::store::Store::open_read_only(
                    state.root.as_path(),
                    &state
                        .store
                        .try_read()
                        .map(|g| g.view.clone())
                        .unwrap_or_default(),
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "view watcher: store reopen failed");
                        continue;
                    }
                };
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
