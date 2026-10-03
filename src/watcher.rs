use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

#[cfg(target_os = "linux")]
use notify::event::CreateKind;
use notify::{Config as NotifyConfig, EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, DebouncedEvent, NoCache, new_debouncer_opt};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::scanner::{CollectObserver, FileResult, ScanError, ScanReport};
use crate::store::Store;

#[derive(Debug, Error)]
pub enum WatchError {
    #[error("notify error: {0}")]
    Notify(#[from] notify::Error),
    #[error("scan error: {0}")]
    Scan(#[from] ScanError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Callback invoked once per processed batch (initial full scan + each debounced batch).
/// Allows main.rs to render results without watcher.rs depending on the renderer.
pub type BatchCallback = Box<dyn FnMut(WatchBatch<'_>) + Send>;

pub struct WatchBatch<'a> {
    pub kind: BatchKind,
    pub report: &'a ScanReport,
    /// The batch's per-file outcomes. Carried explicitly because
    /// [`ScanReport`](crate::scanner::ScanReport) no longer accumulates them: the standalone
    /// watcher renders every line, so it is one of the few callers that genuinely wants the whole
    /// set and opts into a [`CollectObserver`] to get it.
    pub results: &'a [FileResult],
}

#[derive(Debug, Clone, Copy)]
pub enum BatchKind {
    InitialScan,
    /// Paths touched by a debounced batch of file events.
    Incremental {
        paths: usize,
    },
}

/// Bound on the debouncer→consumer queue, in whole debounced batches.
///
/// The consumer rescans synchronously, so on a large monorepo one iteration can take minutes while
/// the debouncer keeps emitting a batch every `watch.debounce_ms` (250 ms by default). Unbounded,
/// that queue grows for as long as the churn lasts — a branch switch across 55 worktrees / 82k files
/// buries the consumer under thousands of `Vec<PathBuf>` batches, each of which costs another rescan.
/// 64 slots is ~16 s of backlog at the default debounce, comfortably more than an incremental scan
/// needs, so the overflow path stays cold in normal operation; past it the producer coalesces into
/// [`PendingPaths`] instead of blocking or dropping.
const DEBOUNCE_QUEUE_CAPACITY: usize = 64;

/// Overflow sink shared by the debouncer callback and the consumer loop.
///
/// A full queue must neither block the callback — it runs on the debouncer's emit thread, so
/// blocking there stalls debouncing and backs up notify's own channel — nor drop the batch, because
/// a lost path means a file silently missing from the index. So the callback folds the overflowing
/// batch into this set: N queued batches collapse into one union of paths, bounded by how many
/// distinct paths the tree has rather than by how many batches the debouncer emitted.
#[derive(Default)]
struct PendingPaths(Mutex<BTreeSet<PathBuf>>);

impl PendingPaths {
    /// Fold one overflowing batch's relevant paths into the set.
    fn coalesce(&self, events: &[DebouncedEvent]) {
        let mut set = self.0.lock().expect("pending paths poisoned");
        for ev in events {
            if !is_relevant(&ev.event.kind) {
                continue;
            }
            set.extend(ev.event.paths.iter().cloned());
        }
    }

    /// Take everything coalesced so far, leaving the set empty. The consumer calls this on every
    /// loop iteration — including the idle timeout — so a set that filled up during a long rescan is
    /// processed as soon as that rescan returns.
    fn take(&self) -> Vec<PathBuf> {
        let mut set = self.0.lock().expect("pending paths poisoned");
        std::mem::take(&mut *set).into_iter().collect()
    }
}

/// Debouncer callback body: hand the batch to the consumer, or coalesce it when the queue is full.
/// Never blocks, never discards a path.
fn dispatch_debounced(
    tx: &std::sync::mpsc::SyncSender<DebounceEventResult>,
    pending: &PendingPaths,
    res: DebounceEventResult,
) {
    match tx.try_send(res) {
        Ok(()) => {}
        Err(std::sync::mpsc::TrySendError::Full(Ok(events))) => {
            debug!(
                n = events.len(),
                "debounce queue full; coalescing batch into the pending set"
            );
            pending.coalesce(&events);
        }
        // Errors carry no path, so there is nothing to coalesce; log them here instead of queueing.
        Err(std::sync::mpsc::TrySendError::Full(Err(errors))) => {
            for e in errors {
                warn!(error = %e, "watch error (debounce queue full)");
            }
        }
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
    }
}

/// Path-emitting primitive at the core of every watcher. Runs the
/// `notify-debouncer-full` event loop and, for each debounced batch, hands the
/// caller the set of repo-relative changed paths (sorted + deduped, with
/// `.basemind/` and out-of-root paths filtered out).
///
/// This is deliberately Store-free and scan-free: it does NOT own a `Store` and
/// never touches the index. Both the standalone `watch` (which owns its own
/// Store and scans) and the embedded MCP serve watcher (which funnels paths into
/// the server's already-open store via `scan_and_refresh`) build on top of it,
/// so we never open a second `.basemind/.lock` flock for the same repo.
///
/// Blocks until `shutdown` fires or the debouncer channel disconnects. No
/// initial signal is emitted: each caller already owns its own initial-scan
/// path, so the callback only ever sees `BatchKind::Incremental` batches.
pub fn watch_paths(
    root: &Path,
    config: &Config,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    on_change: impl FnMut(Vec<PathBuf>, BatchKind),
) -> Result<(), WatchError> {
    watch_paths_reloading(root, config, || None, shutdown, on_change)
}

/// [`watch_paths`] whose indexability filter follows config changes: before each batch is filtered,
/// `reload` is asked for a new config, and a `Some` rebuilds the filter (globs, `[languages]`,
/// floor, submodule roots) from it. The caller owns applying the same config to its own scans, so
/// the filter and the scan can never disagree. Without this the globs and languages the watcher
/// started with stay in force until it is restarted.
pub fn watch_paths_reloading(
    root: &Path,
    config: &Config,
    mut reload: impl FnMut() -> Option<Arc<Config>>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
    mut on_change: impl FnMut(Vec<PathBuf>, BatchKind),
) -> Result<(), WatchError> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<DebounceEventResult>(DEBOUNCE_QUEUE_CAPACITY);
    let pending = Arc::new(PendingPaths::default());
    let pending_producer = Arc::clone(&pending);
    let debounce = Duration::from_millis(config.watch.debounce_ms);
    // ~keep NoCache, not the default RecommendedCache. On macOS/Windows the default is FileIdMap,
    // ~keep whose add_path recursively WalkDirs the whole subtree with follow_links(true) — at
    // ~keep watch() time and again on every dir create/rename — stat-ing every path into an
    // ~keep unbounded HashMap. On a pnpm symlink farm that walk amplifies without bound (issue #43:
    // ~keep 138 MB → 6.85 GB in 3 min). We do our own gitignore-aware filtering in keep_event_path
    // ~keep and re-derive state from disk, so the FileId rename stitching NoCache drops is unused; a
    // ~keep rename just degrades to remove-old + create-new, which scan_paths already handles. Linux
    // ~keep already defaults to NoCache.
    let mut debouncer = new_debouncer_opt::<_, RecommendedWatcher, NoCache>(
        debounce,
        None,
        move |res| dispatch_debounced(&tx, &pending_producer, res),
        NoCache::new(),
        NotifyConfig::default(),
    )?;

    let mut filter = crate::scanner_filter::IndexFilter::new(root, config)?;

    #[cfg(target_os = "linux")]
    {
        let filters = filter.filters();
        // Linux inotify: register watches directory-by-directory, pruning by the same
        // gitignore + exclude-glob filters a full scan uses. This keeps the OS-level watcher
        // from trying to register a watch on a directory we cannot read (permission denied) or
        // that the [scan] exclude globs / .gitignore already drop — the root cause of the
        // "notify error: Permission denied (os error 13)" crash on unreadable trees.
        debouncer.watch(root, RecursiveMode::NonRecursive)?;
        let walker = crate::scanner_filter::ignore_walk_builder(
            root,
            config.scan.respect_gitignore,
            config.scan.follow_symlinks,
        )
        .build();
        for dent in walker {
            // An unreadable directory (e.g. permission denied) surfaces as an iterator error;
            // skip it so inotify never tries to register a watch on it.
            let Ok(entry) = dent else {
                continue;
            };
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if entry.path() == root {
                continue; // root already registered above
            }
            // Skip descending into a directory that the exclude globs or a skipped submodule
            // root already drops.
            let Ok(rel) = entry.path().strip_prefix(root) else {
                continue;
            };
            // On Linux paths use forward slashes, so no backslash normalization is needed.
            if !filters.allows_dir(rel.to_string_lossy().as_ref()) {
                continue;
            }
            // Registering a watch can still fail (the directory was removed between the walk and
            // the watch, or inotify limits were hit). Make it non-fatal: warn and continue.
            if let Err(e) = debouncer.watch(entry.path(), RecursiveMode::NonRecursive) {
                warn!(path = %entry.path().display(), error = %e, "inotify watch registration failed; continuing");
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // macOS (FSEvents) and Windows (ReadDirectoryChangesW) support a single recursive call
        // that auto-registers watches for directories created under an existing watch. The
        // per-directory scheme is a ~10x startup regression on FSEvents (one stream restart per
        // directory), so keep the single recursive call there.
        debouncer.watch(root, RecursiveMode::Recursive)?;
    }

    loop {
        if !matches!(
            shutdown.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ) {
            info!("shutdown requested; exiting watcher");
            return Ok(());
        }
        // One generation of work: the batch we just received, every other batch already queued, and
        // whatever the callback coalesced aside while the last rescan was running. Rescanning those
        // one at a time is what lets a backlog outlive the burst that produced it, so they are merged
        // into a single union of paths instead.
        let mut queued: Vec<Vec<DebouncedEvent>> = Vec::new();
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(events)) => queued.push(events),
            Ok(Err(errors)) => {
                for e in errors {
                    warn!(error = %e, "watch error");
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                info!("debouncer channel closed; exiting watcher");
                return Ok(());
            }
        }
        // Bounded by the queue's own capacity so a sustained event stream can never keep the drain
        // spinning past the next shutdown check.
        for _ in 1..DEBOUNCE_QUEUE_CAPACITY {
            match rx.try_recv() {
                Ok(Ok(events)) => queued.push(events),
                Ok(Err(errors)) => {
                    for e in errors {
                        warn!(error = %e, "watch error");
                    }
                }
                Err(_) => break,
            }
        }

        let mut candidates: Vec<PathBuf> = Vec::new();
        for ev in queued.into_iter().flatten() {
            if !is_relevant(&ev.event.kind) {
                continue;
            }
            // Linux inotify: a directory created after startup is not covered by the
            // frozen NonRecursive watch set. Re-arm by registering a watch on it.
            #[cfg(target_os = "linux")]
            if matches!(&ev.event.kind, EventKind::Create(CreateKind::Folder)) {
                for p in &ev.event.paths {
                    if p.is_dir()
                        && let Err(e) = debouncer.watch(p, RecursiveMode::NonRecursive)
                    {
                        warn!(path = %p.display(), error = %e, "inotify re-arm failed");
                    }
                }
            }
            candidates.extend(ev.event.paths.iter().cloned());
        }
        // Taken after the drain, so a batch that overflowed while we were draining is not left
        // sitting until the next iteration.
        let coalesced = pending.take();
        // A coalesced path has lost its event kind, so the Create(Folder) test above cannot fire for
        // it; re-arm any that is a directory, or a subtree created during the burst stays unwatched.
        #[cfg(target_os = "linux")]
        for p in &coalesced {
            if p.is_dir()
                && let Err(e) = debouncer.watch(p, RecursiveMode::NonRecursive)
            {
                warn!(path = %p.display(), error = %e, "inotify re-arm failed");
            }
        }
        candidates.extend(coalesced);
        if candidates.is_empty() {
            continue;
        }

        if let Some(fresh) = reload() {
            match crate::scanner_filter::IndexFilter::new(root, &fresh) {
                Ok(rebuilt) => {
                    info!("config changed; watcher filter rebuilt");
                    filter = rebuilt;
                }
                Err(error) => warn!(%error, "config changed but its filter does not build; keeping the previous one"),
            }
        }
        filter.clear_cache();
        candidates.sort();
        candidates.dedup();
        let touched: Vec<PathBuf> = candidates
            .into_iter()
            .filter(|p| keep_event_path(&filter, root, p))
            .collect();
        if touched.is_empty() {
            continue;
        }
        debug!(n = touched.len(), "debounced batch");
        let n = touched.len();
        on_change(touched, BatchKind::Incremental { paths: n });
    }
}

/// Run the standalone watcher loop. Blocks until the shutdown receiver fires or
/// the debouncer channel disconnects. Performs an initial full scan, then a thin
/// wrapper over [`watch_paths`] that re-scans only the touched paths via
/// `scanner::scan_paths`.
///
/// This owns its own `Store` and is the backend for the `basemind watch` CLI.
/// The MCP `serve` watcher does NOT use this entry point — it would acquire a
/// second `.basemind/.lock` flock that `serve` already holds. It uses
/// [`watch_paths`] directly and funnels paths into serve's open store instead.
pub fn watch(
    root: &Path,
    store: Arc<Mutex<Store>>,
    config: Arc<Config>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    mut on_batch: BatchCallback,
) -> Result<(), WatchError> {
    info!(root = %root.display(), "initial scan");
    {
        let mut guard = store.lock().expect("store poisoned");
        let mut observer = CollectObserver::new();
        let report = crate::scanner::scan_with_observer(
            root,
            &mut guard,
            &config,
            crate::scanner::ScanSource::WorkingTree,
            crate::scanner::EmbedMode::Inline,
            &crate::scanner::ScanCancel::new(),
            &mut observer,
        )?;
        on_batch(WatchBatch {
            kind: BatchKind::InitialScan,
            report: &report,
            results: observer.results(),
        });
    }
    info!("initial scan complete; entering watch mode");

    // The config the filter and the scans both use; replaced together when `basemind.toml` changes.
    let current = Arc::new(Mutex::new(Arc::clone(&config)));
    let reload_current = Arc::clone(&current);
    let mut reloader = config_reloader(root);
    watch_paths_reloading(
        root,
        &config,
        move || {
            let fresh = reloader()?;
            *reload_current.lock().expect("config poisoned") = Arc::clone(&fresh);
            Some(fresh)
        },
        shutdown,
        |touched, kind| {
            let config = Arc::clone(&current.lock().expect("config poisoned"));
            let mut guard = store.lock().expect("store poisoned");
            let mut observer = CollectObserver::new();
            match crate::scanner::scan_paths_with_observer(
                root,
                &mut guard,
                &config,
                &touched,
                crate::scanner::EmbedMode::Inline,
                &crate::scanner::ScanCancel::new(),
                &mut observer,
            ) {
                Ok(report) => {
                    on_batch(WatchBatch {
                        kind,
                        report: &report,
                        results: observer.results(),
                    });
                }
                Err(e) => warn!(error = %e, "scan_paths failed"),
            }
        },
    )
}

/// A `reload` source for [`watch_paths_reloading`] that yields a freshly loaded config whenever the
/// workspace's `basemind.toml` changed since the previous call. A file that stops loading is
/// reported once and the previous config stays in force.
pub fn config_reloader(root: &Path) -> impl FnMut() -> Option<Arc<Config>> + use<> {
    let root = root.to_path_buf();
    let mut stamp = crate::config::daemon::ConfigStamp::of(&root);
    move || {
        let now = crate::config::daemon::ConfigStamp::of(&root);
        if now == stamp {
            return None;
        }
        stamp = now;
        match crate::config::load_with_overrides(&root, None, None) {
            Ok(loaded) => Some(Arc::new(loaded.config)),
            Err(error) => {
                warn!(%error, "config changed but does not load; keeping the previous config");
                None
            }
        }
    }
}

fn is_relevant(kind: &EventKind) -> bool {
    matches!(kind, EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_))
}

/// Should this event path wake a rescan? Keep only what a full scan would index. For an existing
/// path that means include/exclude globs AND the nested-`.gitignore` hierarchy; for a deleted path
/// (gone from disk, so gitignore can't be evaluated) keep anything the glob layer allows so a
/// previously-indexed file is still forwarded for pruning. Out-of-root and empty/ancestor rels
/// (the FSEvents coalescing case) are dropped.
fn keep_event_path(filter: &crate::scanner_filter::IndexFilter, root: &Path, p: &Path) -> bool {
    let Ok(rel) = p.strip_prefix(root) else {
        return false;
    };
    if rel.components().any(|c| c.as_os_str() == crate::config::BASEMIND_DIR) {
        return false;
    }
    let rel_cow = rel.to_string_lossy();
    let rel_normalized;
    let rel: &str = if rel_cow.contains('\\') {
        rel_normalized = rel_cow.replace('\\', "/");
        &rel_normalized
    } else {
        &rel_cow
    };
    if rel.is_empty() {
        return false;
    }
    if p.exists() {
        filter.is_indexable(p)
    } else {
        filter.allows_glob(rel)
    }
}

#[cfg(test)]
#[path = "watcher_tests.rs"]
mod tests;
