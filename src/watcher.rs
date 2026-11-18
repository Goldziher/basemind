use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use notify::event::ModifyKind;
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
    /// loop iteration, so a set that filled up during a long rescan is processed as soon as that
    /// rescan returns.
    fn take(&self) -> Vec<PathBuf> {
        let mut set = self.0.lock().expect("pending paths poisoned");
        std::mem::take(&mut *set).into_iter().collect()
    }
}

/// What the consumer loop receives: a debounced batch, or the caller's shutdown. Shutdown rides the
/// same queue so the loop can block on it instead of polling for the signal.
enum WatchMsg {
    Batch(DebounceEventResult),
    Shutdown,
}

/// Debouncer callback body: hand the batch to the consumer, or coalesce it when the queue is full.
/// Never blocks, never discards a path.
fn dispatch_debounced(tx: &std::sync::mpsc::SyncSender<WatchMsg>, pending: &PendingPaths, res: DebounceEventResult) {
    match tx.try_send(WatchMsg::Batch(res)) {
        Ok(()) => {}
        Err(std::sync::mpsc::TrySendError::Full(WatchMsg::Batch(Ok(events)))) => {
            debug!(
                n = events.len(),
                "debounce queue full; coalescing batch into the pending set"
            );
            pending.coalesce(&events);
        }
        // Errors carry no path, so there is nothing to coalesce; log them here instead of queueing.
        Err(std::sync::mpsc::TrySendError::Full(WatchMsg::Batch(Err(errors)))) => {
            for e in errors {
                warn!(error = %e, "watch error (debounce queue full)");
            }
        }
        Err(
            std::sync::mpsc::TrySendError::Full(WatchMsg::Shutdown) | std::sync::mpsc::TrySendError::Disconnected(_),
        ) => {}
    }
}

/// Forward `shutdown` into the consumer's queue from a thread of its own, which sleeps until the
/// signal fires or its sender is dropped; either ends the watcher.
fn forward_shutdown(
    shutdown: tokio::sync::oneshot::Receiver<()>,
    tx: std::sync::mpsc::SyncSender<WatchMsg>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("basemind-watch-stop".to_string())
        .spawn(move || {
            let _ = shutdown.blocking_recv();
            let _ = tx.send(WatchMsg::Shutdown);
        })
        .map(drop)
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
    shutdown: tokio::sync::oneshot::Receiver<()>,
    mut on_change: impl FnMut(Vec<PathBuf>, BatchKind),
) -> Result<(), WatchError> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<WatchMsg>(DEBOUNCE_QUEUE_CAPACITY);
    let stop_tx = tx.clone();
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
    let mut dir_watches = DirWatches::default();
    #[cfg(target_os = "linux")]
    {
        let filters = filter.filters();
        // Linux inotify: register watches directory-by-directory, pruning by the same
        // gitignore + exclude-glob filters a full scan uses. This keeps the OS-level watcher
        // from trying to register a watch on a directory we cannot read (permission denied) or
        // that the [scan] exclude globs / .gitignore already drop — the root cause of the
        // "notify error: Permission denied (os error 13)" crash on unreadable trees.
        debouncer.watch(root, RecursiveMode::NonRecursive)?;
        dir_watches.watched.insert(root.to_path_buf());
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
            match debouncer.watch(entry.path(), RecursiveMode::NonRecursive) {
                Ok(()) => {
                    dir_watches.watched.insert(entry.path().to_path_buf());
                }
                Err(e) => {
                    warn!(path = %entry.path().display(), error = %e, "inotify watch registration failed; continuing");
                }
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

    forward_shutdown(shutdown, stop_tx)?;

    loop {
        // One generation of work: the batch we just received, every other batch already queued, and
        // whatever the callback coalesced aside while the last rescan was running. Rescanning those
        // one at a time is what lets a backlog outlive the burst that produced it, so they are merged
        // into a single union of paths instead.
        let mut queued: Vec<Vec<DebouncedEvent>> = Vec::new();
        // Blocks: an idle watcher wakes only for a batch or the shutdown, never on a timer.
        let Ok(first) = rx.recv() else {
            info!("watch channel closed; exiting watcher");
            return Ok(());
        };
        // Bounded by the queue's own capacity so a sustained event stream cannot keep the drain
        // spinning while a shutdown waits behind it.
        let rest = (1..DEBOUNCE_QUEUE_CAPACITY).map_while(|_| rx.try_recv().ok());
        for message in std::iter::once(first).chain(rest) {
            match message {
                WatchMsg::Batch(Ok(events)) => queued.push(events),
                WatchMsg::Batch(Err(errors)) => {
                    for e in errors {
                        warn!(error = %e, "watch error");
                    }
                }
                WatchMsg::Shutdown => {
                    info!("shutdown requested; exiting watcher");
                    return Ok(());
                }
            }
        }

        let mut candidates: Vec<PathBuf> = Vec::new();
        // Paths whose event can mean a directory appeared under a new name: only these are expanded
        // into their subtree when they turn out to be a directory, so a metadata change on a
        // directory does not rescan everything beneath it.
        let mut dir_events: BTreeSet<PathBuf> = BTreeSet::new();
        for ev in queued.into_iter().flatten() {
            if !is_relevant(&ev.event.kind) {
                continue;
            }
            if may_bring_a_directory(&ev.event.kind) {
                dir_events.extend(ev.event.paths.iter().cloned());
            }
            candidates.extend(ev.event.paths.iter().cloned());
        }
        // Taken after the drain, so a batch that overflowed while we were draining is not left
        // sitting until the next iteration. A coalesced path has lost its event kind, so it is
        // treated as one that may bring a directory.
        let coalesced = pending.take();
        dir_events.extend(coalesced.iter().cloned());
        candidates.extend(coalesced);
        if candidates.is_empty() {
            continue;
        }
        candidates.sort();
        candidates.dedup();

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
        // Linux inotify watches one directory at a time. Release every watch at or under a path
        // that is no longer a directory (removed, or renamed or moved away: the kernel watch follows
        // the inode, so it would keep reporting under the old name), then arm every directory that
        // appeared, with its whole subtree: a directory renamed or moved in, or one filled before
        // its own create event was handled, has subdirectories no event named.
        #[cfg(target_os = "linux")]
        dir_watches.sync(&mut debouncer, &filter, &candidates);
        let touched: Vec<PathBuf> = candidates
            .into_iter()
            .filter(|p| keep_event_path(&filter, root, p, dir_events.contains(p)))
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
        match crate::config::overrides::load_with_overrides(&root, None, None) {
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

/// Can an event of this kind mean a directory now exists at its path: one created, or renamed or
/// moved into place? FSEvents and inotify report a directory rename on the directory alone.
fn may_bring_a_directory(kind: &EventKind) -> bool {
    matches!(kind, EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_)))
}

/// What [`DirWatches`] arms and releases through: the debouncer in production, a recorder in tests.
#[cfg(any(target_os = "linux", test))]
trait DirRegistry {
    fn watch_dir(&mut self, dir: &Path) -> Result<(), notify::Error>;
    fn unwatch_dir(&mut self, dir: &Path);
}

#[cfg(target_os = "linux")]
impl DirRegistry for notify_debouncer_full::Debouncer<RecommendedWatcher, NoCache> {
    fn watch_dir(&mut self, dir: &Path) -> Result<(), notify::Error> {
        self.watch(dir, RecursiveMode::NonRecursive)
    }

    fn unwatch_dir(&mut self, dir: &Path) {
        let _ = self.unwatch(dir);
    }
}

/// The directories the Linux watcher holds a non-recursive watch on. Kept so a directory that goes
/// away releases the watches of its whole subtree, and so a directory already armed is not walked
/// again on every event it produces.
#[cfg(any(target_os = "linux", test))]
#[derive(Default)]
struct DirWatches {
    watched: BTreeSet<PathBuf>,
}

#[cfg(any(target_os = "linux", test))]
impl DirWatches {
    /// Bring the watch set in line with the batch's `paths`: release first, then arm, so a
    /// directory renamed within the tree drops the watch that still reports under its old name
    /// before its new name is armed.
    fn sync(
        &mut self,
        registry: &mut impl DirRegistry,
        filter: &crate::scanner_filter::IndexFilter,
        paths: &[PathBuf],
    ) {
        for p in paths {
            let is_dir = std::fs::symlink_metadata(p).is_ok_and(|meta| meta.is_dir());
            if !is_dir {
                self.release_tree(registry, p);
            }
        }
        for p in paths {
            self.arm_tree(registry, filter, p);
        }
    }

    /// Release the watch on `gone` and on every watched directory beneath it.
    fn release_tree(&mut self, registry: &mut impl DirRegistry, gone: &Path) {
        let beneath: Vec<PathBuf> = self
            .watched
            .range(gone.to_path_buf()..)
            .take_while(|p| p.starts_with(gone))
            .cloned()
            .collect();
        for dir in beneath {
            registry.unwatch_dir(&dir);
            self.watched.remove(&dir);
        }
    }

    /// Arm `dir` and every watchable directory beneath it, unless `dir` is already armed.
    fn arm_tree(&mut self, registry: &mut impl DirRegistry, filter: &crate::scanner_filter::IndexFilter, dir: &Path) {
        if self.watched.contains(dir) || !filter.is_watchable_dir(dir) {
            return;
        }
        for sub in filter.subtree_dirs(dir) {
            if self.watched.contains(&sub) {
                continue;
            }
            match registry.watch_dir(&sub) {
                Ok(()) => {
                    self.watched.insert(sub);
                }
                Err(e) => warn!(path = %sub.display(), error = %e, "inotify re-arm failed"),
            }
        }
    }
}

/// Should this event path wake a rescan? Keep only what a full scan would index. For an existing
/// path that means include/exclude globs AND the nested-`.gitignore` hierarchy; a directory is kept
/// when its event may have brought it under this name (`dir_event`) and a full scan would descend
/// into it, so the scan expands it into its files; for a deleted path
/// (gone from disk, so gitignore can't be evaluated) keep anything the glob layer allows so a
/// previously-indexed file is still forwarded for pruning. Out-of-root and empty/ancestor rels
/// (the FSEvents coalescing case) are dropped. Resolver config files (`tsconfig*.json`,
/// `package.json`) are kept even when not indexed, since an importer's resolution may depend on them.
fn keep_event_path(filter: &crate::scanner_filter::IndexFilter, root: &Path, p: &Path, dir_event: bool) -> bool {
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
    if std::fs::symlink_metadata(p).is_ok_and(|meta| meta.is_dir()) {
        dir_event && filter.is_watchable_dir(p)
    } else if p.exists() {
        filter.is_indexable(p) || (is_resolver_config(rel) && filter.allows_glob(rel))
    } else {
        filter.allows_glob(rel)
    }
}

/// Module-resolution config a resolver reads without indexing it. Editing one can change what every
/// importer below it resolves to, so the event is forwarded and the scan decides (against the probe
/// index) whether anything depends on it.
fn is_resolver_config(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name == "package.json" || name == "jsconfig.json" || (name.starts_with("tsconfig") && name.ends_with(".json"))
}

#[cfg(test)]
#[path = "watcher_tests.rs"]
mod tests;
