//! Detached background facilities spawned by `serve`: the initial scan, the map warm, blob GC and
//! the two filesystem watchers. Each spawns a thread that runs a synchronous engine operation
//! ([`Workspace::rescan`], [`Workspace::warm`], [`Workspace::gc_blobs`]) and logs its outcome; a
//! daemon-hosted workspace's scans go to the daemon's pool instead
//! ([`writer_rescan`](super::daemon_forward::writer_rescan)).
//!
//! The threads are the runtime's blocking pool, so a runtime shutting down waits for the operation
//! in flight and starts no later one.

use std::sync::Arc;
use std::time::Duration;

use super::{MapCache, ServerState};
use crate::engine::{EngineError, Workspace};
use crate::scanner::EmbedMode;
use crate::watcher::BatchKind;

/// Run `work` on the runtime's blocking pool, logging `panicked` if it panics.
fn spawn_logged(panicked: &'static str, work: impl FnOnce() + Send + 'static) {
    let task = tokio::task::spawn_blocking(work);
    tokio::spawn(async move {
        if let Err(error) = task.await
            && error.is_panic()
        {
            tracing::error!(%error, "{panicked}");
        }
    });
}

/// Run an in-process blob GC once on a blocking thread, logging the outcome and swallowing any error.
pub(super) fn spawn_background_gc(ws: Arc<Workspace>) {
    spawn_logged("background blob GC task panicked", move || run_background_gc(&ws));
}

/// [`Workspace::gc_blobs`], logged.
fn run_background_gc(ws: &Workspace) {
    match ws.gc_blobs() {
        Ok(None) => tracing::debug!("background blob GC skipped: blob cache is shared across git worktrees"),
        Ok(Some(report)) if report.removed > 0 => tracing::info!(
            removed = report.removed,
            bytes_freed = report.bytes_freed,
            "background blob GC reclaimed orphaned blobs"
        ),
        Ok(Some(_)) => tracing::debug!("background blob GC: nothing to reclaim"),
        Err(error) => tracing::warn!(%error, "background blob GC failed"),
    }
}

/// Where a workspace's scans run: its own store, or a daemon-hosted workspace's pool writer.
#[derive(Clone)]
struct Writer {
    ws: Arc<Workspace>,
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pool: Option<Arc<crate::comms::workspace_pool::WorkspacePool>>,
}

impl Writer {
    fn of(state: &ServerState) -> Self {
        Self {
            ws: Arc::clone(&state.shared),
            #[cfg(all(feature = "comms", any(unix, windows)))]
            pool: state.pool.clone(),
        }
    }

    /// Whether the daemon's pool writes this workspace.
    fn hosted(&self) -> bool {
        #[cfg(all(feature = "comms", any(unix, windows)))]
        return self.pool.is_some();
        #[cfg(not(all(feature = "comms", any(unix, windows))))]
        false
    }

    /// Scan `paths` (`None`: the whole working tree), not named by anyone, and report `(scanned,
    /// updated, removed)`.
    fn scan(
        &self,
        paths: Option<Vec<std::path::PathBuf>>,
        embed: EmbedMode,
    ) -> Result<(usize, usize, usize), EngineError> {
        #[cfg(all(feature = "comms", any(unix, windows)))]
        if let Some(pool) = &self.pool {
            let embed = matches!(embed, EmbedMode::Inline);
            let stats = super::daemon_forward::writer_rescan(&self.ws, pool, paths, false, embed, false)?;
            return Ok((stats.scanned, stats.updated, stats.removed));
        }
        let report = self.ws.rescan(paths, embed, false)?;
        Ok((report.stats.scanned, report.stats.updated, report.stats.removed))
    }

    /// Whether the catch-up should stop before its next step: the daemon is draining, or the pool no
    /// longer holds the workspace (evicted while the map warmed), which a scan would reopen only to
    /// publish to a stack nobody serves.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    fn catch_up_moot(&self) -> bool {
        if let Some(pool) = &self.pool {
            let key = crate::store::workspace_key(&self.ws.root);
            return pool.scan_cancel().is_cancelled() || !pool.accessed().iter().any(|hot| hot.key == key);
        }
        false
    }

    /// A whole-tree pass that judges every file by its stat and hash, as `index_rescan {}` does:
    /// the boot catch-up ([`spawn_catch_up`], without embedding) and the serve watcher's answer to
    /// events the OS dropped. Reported as [`scan`](Self::scan) reports, or `None` when it was
    /// cancelled. A hosted stack then takes the pool's file map only when it differs from the one it
    /// serves ([`Workspace::publish_store_if_changed`]): a publish bumps the generations every cursor
    /// is bound to, and an unchanged tree must not cut a session's paging short. The comparison is
    /// with the map, not the pass's counts, so a write another path made since the stack opened is
    /// taken.
    fn tree_pass(&self, embed: EmbedMode) -> Result<Option<(usize, usize, usize)>, EngineError> {
        #[cfg(all(feature = "comms", any(unix, windows)))]
        if let Some(pool) = &self.pool {
            let embed = matches!(embed, EmbedMode::Inline);
            let (stats, cancelled) = pool
                .hosted_rescan(&self.ws.root, None, false, embed, false)
                .map_err(|error| EngineError::internal(format!("host rescan: {error}")))?;
            if cancelled {
                return Ok(None);
            }
            let store = pool
                .read_store(&self.ws.root)
                .map_err(|error| EngineError::internal(format!("reopen read-only store: {error}")))?;
            if let Some(cache) = self.ws.publish_store_if_changed(store) {
                warm_terms(&cache);
            }
            return Ok(Some((stats.scanned, stats.updated, stats.removed)));
        }
        self.scan(None, embed).map(Some)
    }
}

/// Boot-time initial index build for an empty index, spawned once from `BasemindServer::new`.
///
/// Two passes so serve becomes queryable fast without pinning the machine on ONNX embedding:
/// 1. A `Deferred` scan writes the code-map + BM25 keyword lane + content-addressed blobs but NO
///    embeddings — this is what clears `initial_scan_active`, so `status` reports the index ready.
/// 2. A detached `Inline` scan then fills the vectors the fast pass skipped, reusing the fast pass'
///    content-addressed caches (only not-yet-embedded content is embedded, bounded by WS4-A's embed
///    pool). GC runs after it settles so the sweep reaps against the final blob set; a hosted
///    workspace's pool owns its sweep.
pub(super) fn spawn_initial_scan(state: Arc<ServerState>) {
    tracing::info!("empty index on startup; running initial scan in background");
    let writer = Writer::of(&state);
    spawn_logged("initial background scan task panicked", move || {
        use std::sync::atomic::Ordering;
        let ws = &writer.ws;
        ws.initial_scan_active.store(true, Ordering::Relaxed);
        let started = std::time::Instant::now();
        match writer.scan(None, EmbedMode::Deferred) {
            Ok((scanned, updated, _)) if writer.hosted() => tracing::info!(
                scanned,
                updated,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "initial scan complete (via daemon writer; embeddings deferred)"
            ),
            Ok((scanned, updated, _)) => tracing::info!(
                scanned,
                updated,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "initial background scan complete (code-map + keyword lane; embeddings deferred)"
            ),
            Err(error) if writer.hosted() => tracing::warn!(%error, "initial writer scan failed"),
            Err(error) => tracing::warn!(%error, "initial background scan failed"),
        }
        ws.initial_scan_ms
            .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        ws.initial_scan_active.store(false, Ordering::Relaxed);
        spawn_logged("background embedding pass task panicked", move || {
            let embed_started = std::time::Instant::now();
            let hosted = writer.hosted();
            if hosted {
                tracing::info!("background embedding pass starting (forwarded to daemon)");
            } else {
                tracing::info!("background embedding pass starting");
            }
            match writer.scan(None, EmbedMode::Inline) {
                Ok((scanned, updated, _)) if hosted => tracing::info!(
                    scanned,
                    updated,
                    elapsed_ms = embed_started.elapsed().as_millis() as u64,
                    "background embedding pass complete (via daemon writer)"
                ),
                Ok((scanned, updated, _)) => tracing::info!(
                    scanned,
                    updated,
                    elapsed_ms = embed_started.elapsed().as_millis() as u64,
                    "background embedding pass complete"
                ),
                Err(error) if hosted => tracing::warn!(%error, "background writer embedding pass failed"),
                Err(error) => tracing::warn!(%error, "background embedding pass failed"),
            }
            if !hosted {
                run_background_gc(&writer.ws);
            }
        });
    });
}

/// Boot-time catch-up of a daemon-hosted stack built over a populated index
/// ([`BootScan::CatchUp`](crate::engine::workspace::BootScan::CatchUp)): the edits made while no
/// watcher ran (the stack shed, the daemon gone, a `git checkout` in between) are indexed without
/// anyone calling `index_rescan`.
///
/// The map warm (`warm_first`) runs first so the pass refreshes a full map, then a whole-tree pass
/// through the pool, which judges every file by its stat and hash as `index_rescan {}` does and
/// coalesces with a client's rescan of the same tree. The workspace reports `rescanning` from the
/// moment the stack is served until that pass has published its file map; a pass that changed
/// anything is followed, as the initial scan is, by a detached embedding pass. The host's work pin,
/// when it lends one (a workspace with a git-history index), keeps the daemon up meanwhile; the
/// pool's drain token stops the pass, and a pass the daemon no longer needs is skipped.
#[cfg(all(feature = "comms", any(unix, windows)))]
pub(super) fn spawn_catch_up(state: Arc<ServerState>, warm_first: bool) {
    let writer = Writer::of(&state);
    let rescanning = writer.ws.mark_rescan();
    let pin = writer.ws.work_pin.as_ref().map(|pin| pin());
    spawn_logged("catch-up scan task panicked", move || {
        if warm_first {
            writer.ws.warm();
        }
        if writer.catch_up_moot() {
            tracing::debug!("catch-up skipped: the daemon is draining or the workspace was evicted");
            return;
        }
        let started = std::time::Instant::now();
        let outcome = writer.tree_pass(EmbedMode::Deferred);
        let changed = match outcome {
            Ok(None) => {
                tracing::debug!("catch-up cancelled: the daemon is draining");
                false
            }
            Ok(Some((scanned, updated, removed))) => {
                tracing::info!(
                    scanned,
                    updated,
                    removed,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "catch-up scan complete"
                );
                updated + removed > 0
            }
            Err(error) => {
                tracing::warn!(%error, "catch-up scan failed");
                false
            }
        };
        if changed {
            spawn_logged("catch-up embedding pass task panicked", move || {
                let _pin = pin;
                if let Err(error) = writer.scan(None, EmbedMode::Inline) {
                    tracing::warn!(%error, "catch-up embedding pass failed");
                }
            });
        } else {
            drop(writer);
        }
        // Last: a workspace that reads settled holds no reference from this pass. ~keep
        drop(rescanning);
    });
}

/// Boot-time in-RAM code-map preload, spawned once from `BasemindServer::new_with_options` when the
/// index is already populated (no initial scan needed) on the background `serve` path.
///
/// `serve` boots with an EMPTY [`MapCache`] placeholder so it can answer the MCP `initialize` /
/// `tools/list` handshake immediately, then [`Workspace::warm`] does the heavy `MapCache::build` (a
/// rayon `par_iter` over every L1/L2 blob) on a blocking thread, publishes the full map, and opens
/// the warm gate every tool waits on in [`ServerState::await_cache_ready`]. Without this, that build
/// ran synchronously before `.serve(transport)` and — under rayon-pool contention from other
/// sessions' scans — could take minutes, blowing the client's startup window so the tools never
/// registered.
pub(super) fn spawn_cache_warm(state: Arc<ServerState>) {
    tracing::info!("warming in-RAM code map in background (handshake already served)");
    let ws = Arc::clone(&state.shared);
    spawn_logged(
        "in-RAM code map warm task panicked; serving un-warmed cache",
        move || ws.warm(),
    );
}

/// Build the term index on a blocking thread so the first `symbols` / `dependents` call after
/// a full cache publish doesn't pay for it. A no-op outside a Tokio runtime (queries then build
/// it lazily).
pub(super) fn warm_terms(cache: &Arc<MapCache>) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        let cache = Arc::clone(cache);
        runtime.spawn_blocking(move || {
            cache.terms();
        });
    }
}

const VIEW_WATCHER_DEBOUNCE: Duration = Duration::from_millis(150);
const VIEW_WATCHER_SHUTDOWN_POLL: Duration = Duration::from_millis(200);

/// Bound on the view watcher's debouncer→consumer queue, in whole debounced batches.
///
/// A MapCache rebuild is synchronous and takes as long as reopening the index plus rebuilding the
/// map, so a writer rewriting the index in a loop can outrun this consumer; unbounded, every one of
/// those rewrites would sit in the queue holding its event paths. 8 slots is ~1.2 s of backlog at
/// [`VIEW_WATCHER_DEBOUNCE`] and the consumer collapses whatever it finds into a single rebuild
/// anyway, so a deeper queue would only buy duplicate work. Overflow sets `pending_touch` instead of
/// blocking the debouncer's emit thread or dropping the notification — a dropped index rewrite would
/// leave the served MapCache permanently stale.
const VIEW_WATCHER_QUEUE_CAPACITY: usize = 8;

/// Does a debounced batch touch the view's index file? That one path is the whole of the view
/// watcher's event filtering, so it runs both in the consumer and — on queue overflow — in the
/// debouncer callback.
fn touches_index(events: &[notify_debouncer_full::DebouncedEvent], target: &std::path::Path) -> bool {
    events
        .iter()
        .any(|de| de.event.paths.iter().any(|p| p.as_path() == target))
}

/// Drop guard that requests shutdown of a filesystem watcher.
///
/// The request is asynchronous: the watcher thread hears it through the debouncer's queue, stops
/// its OS stream and only then releases the server state it holds, which on a loaded machine is
/// seconds after the drop. [`exited`](Self::exited) is the point it has.
pub(super) struct WatcherGuard {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    /// Set by the drop, before the thread can hear the shutdown: a batch it takes from then on is
    /// not scanned. The scan would reopen a workspace the pool just evicted, holding its writer lock
    /// again, only to publish to a stack nobody serves.
    stopping: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(all(test, feature = "comms", any(unix, windows)))]
    exited: tokio::sync::watch::Receiver<()>,
}

/// The watcher thread's half of a [`WatcherGuard`]: dropped as the thread's last act, after
/// everything else it held.
type WatcherExit = tokio::sync::watch::Sender<()>;

/// Resolves once a watcher thread has exited and released what it held.
#[cfg(all(test, feature = "comms", any(unix, windows)))]
#[derive(Clone)]
pub(crate) struct WatcherExited(tokio::sync::watch::Receiver<()>);

#[cfg(all(test, feature = "comms", any(unix, windows)))]
impl WatcherExited {
    pub(crate) async fn wait(mut self) {
        while self.0.changed().await.is_ok() {}
    }
}

impl WatcherGuard {
    fn new() -> (Self, tokio::sync::oneshot::Receiver<()>, WatcherExit) {
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (exit, _exited) = tokio::sync::watch::channel(());
        let guard = Self {
            shutdown: Some(shutdown),
            stopping: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(all(test, feature = "comms", any(unix, windows)))]
            exited: _exited,
        };
        (guard, shutdown_rx, exit)
    }

    #[cfg(all(test, feature = "comms", any(unix, windows)))]
    pub(super) fn exited(&self) -> WatcherExited {
        WatcherExited(self.exited.clone())
    }
}

impl Drop for WatcherGuard {
    fn drop(&mut self) {
        self.stopping.store(true, std::sync::atomic::Ordering::Release);
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

/// Spawn a working-tree watcher whose lifetime is controlled by the returned guard.
///
/// Unlike [`spawn_view_watcher`], this watches source paths and bridges each debounced refresh from
/// a blocking OS thread back through the current Tokio runtime. Dropping the guard requests
/// shutdown and releases the watcher-owned server state.
pub(super) fn spawn_serve_watcher(state: Arc<ServerState>) -> WatcherGuard {
    let (guard, shutdown_rx, exit) = WatcherGuard::new();
    spawn_serve_watcher_thread(state, shutdown_rx, Arc::clone(&guard.stopping), exit);
    guard
}

fn spawn_serve_watcher_thread(
    state: Arc<ServerState>,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    stopping: Arc<std::sync::atomic::AtomicBool>,
    exit: WatcherExit,
) {
    let writer = Writer::of(&state);
    drop(state);
    let root = writer.ws.root.clone();
    let config = Arc::clone(&writer.ws.config);
    // ~keep A hosted scan warms the published map's term index on the runtime's blocking pool.
    let handle = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("basemind-mcp-serve-watcher".to_string())
        .spawn(move || {
            let _runtime = handle.enter();
            tracing::info!(root = %root.display(), "serve watcher armed (live incremental rescan)");
            let health = &writer.ws.watch_health;
            let result = crate::watcher::watch_paths(&root, &config, health, shutdown_rx, |paths, kind| {
                if stopping.load(std::sync::atomic::Ordering::Acquire) {
                    tracing::debug!("serve watcher: stopping; batch not scanned");
                    return;
                }
                let rescanning = writer.ws.mark_rescan();
                let outcome = match kind {
                    BatchKind::FullRescan => writer.tree_pass(EmbedMode::Inline),
                    _ => writer.scan(Some(paths), EmbedMode::Inline).map(Some),
                };
                drop(rescanning);
                match outcome {
                    Ok(None) => tracing::debug!("serve watcher: whole-tree rescan cancelled: the daemon is draining"),
                    Ok(Some((scanned, updated, removed))) => {
                        tracing::debug!(scanned, updated, removed, "serve watcher: incremental rescan complete")
                    }
                    Err(error) => tracing::warn!(
                        %error,
                        "serve watcher: incremental rescan failed (watcher continues)"
                    ),
                }
            });
            health.clear();
            if let Err(error) = result {
                tracing::warn!(%error, "serve watcher exited with error");
            }
            tracing::info!("serve watcher: exiting");
            drop(writer);
            drop(exit);
        })
        .ok();
}

/// Reopen the working store read-only for a MapCache rebuild. Only a read-only `serve` runs this
/// watcher; a daemon-hosted workspace refreshes after each pool scan instead.
fn reopen_read_only(state: &ServerState, view: &str) -> Result<crate::store::Store, crate::store::StoreError> {
    crate::store::Store::open_read_only(state.shared.root.as_path(), view)
}

/// Spawn a passive index watcher whose lifetime is controlled by the returned guard.
pub(super) fn spawn_view_watcher(state: Arc<ServerState>) -> WatcherGuard {
    let (guard, mut shutdown_rx, exit) = WatcherGuard::new();
    let (basemind_dir, view) = {
        let store = match state.shared.try_store_read() {
            Ok(g) => g,
            Err(_) => return guard,
        };
        (store.basemind_dir.clone(), store.view.clone())
    };
    let view_dir = basemind_dir.join(crate::store::VIEWS_DIR).join(&view);
    let target = view_dir.join(crate::store::INDEX_FILE);

    std::thread::Builder::new()
        .name("basemind-mcp-view-watcher".to_string())
        .spawn(move || {
            use notify::RecommendedWatcher;
            use notify_debouncer_full::{DebounceEventResult, NoCache, new_debouncer_opt};
            use std::sync::atomic::{AtomicBool, Ordering};
            // Bound before `state`, so it drops after it on every return. ~keep
            let _exit = exit;
            let state = state;

            let (tx, rx) = std::sync::mpsc::sync_channel::<DebounceEventResult>(VIEW_WATCHER_QUEUE_CAPACITY);
            // Overflow state for a full queue: one bit, because every batch that reaches `target`
            // asks for the same single rebuild. Set from the debouncer's emit thread, which must
            // never block on this queue.
            let pending_touch = Arc::new(AtomicBool::new(false));
            let touch_producer = Arc::clone(&pending_touch);
            let target_producer = target.clone();
            let handler = move |result: DebounceEventResult| {
                if let Err(std::sync::mpsc::TrySendError::Full(Ok(events))) = tx.try_send(result)
                    && touches_index(&events, &target_producer)
                {
                    touch_producer.store(true, Ordering::Relaxed);
                }
            };
            // ~keep NoCache, not the default FileIdMap — see src/watcher.rs (issue #43). We only
            // ~keep compare event paths to `target`, so the FileId rename cache is dead weight here.
            let mut debouncer = match new_debouncer_opt::<_, RecommendedWatcher, NoCache>(
                VIEW_WATCHER_DEBOUNCE,
                None,
                handler,
                NoCache::new(),
                notify::Config::default(),
            ) {
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

            loop {
                if !matches!(
                    shutdown_rx.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                ) {
                    break;
                }
                let mut rebuild = false;
                match rx.recv_timeout(VIEW_WATCHER_SHUTDOWN_POLL) {
                    Ok(Ok(events)) => rebuild |= touches_index(&events, &target),
                    Ok(Err(_)) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                // Collapse the rest of the queue into this iteration: N index rewrites that piled up
                // during the last rebuild still need exactly one rebuild.
                for _ in 1..VIEW_WATCHER_QUEUE_CAPACITY {
                    match rx.try_recv() {
                        Ok(Ok(events)) => rebuild |= touches_index(&events, &target),
                        Ok(Err(_)) => {}
                        Err(_) => break,
                    }
                }
                // Whatever overflowed while the last rebuild ran. Cleared as it is read, and read on
                // every iteration (the idle timeout included) so it can never be stranded.
                rebuild |= pending_touch.swap(false, Ordering::Relaxed);
                if !rebuild {
                    continue;
                }
                let view = state
                    .shared
                    .try_store_read()
                    .map(|g| g.view.clone())
                    .unwrap_or_default();
                let new_store = match reopen_read_only(&state, &view) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "view watcher: store reopen failed");
                        continue;
                    }
                };
                let budget = super::l1_cache::budget_bytes_from(&state.shared.config.resources);
                let Some(new_cache) = state.shared.cache.load_full().refreshed(&new_store, budget) else {
                    tracing::debug!("view watcher: index rewritten but unchanged; keeping the current MapCache");
                    continue;
                };
                let new_cache = Arc::new(new_cache);
                tracing::info!(
                    files = new_cache.len(),
                    "view watcher: refreshed MapCache from the rewritten index"
                );
                state.shared.cache.store(new_cache);
                state.shared.note_index_changed();
            }
            tracing::info!("view watcher: channel closed; exiting");
        })
        .ok();
    guard
}
