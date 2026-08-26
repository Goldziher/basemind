use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

#[cfg(target_os = "linux")]
use notify::event::CreateKind;
use notify::{Config as NotifyConfig, EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, NoCache, new_debouncer_opt};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::scanner::{ScanError, ScanReport};
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
}

#[derive(Debug, Clone, Copy)]
pub enum BatchKind {
    InitialScan,
    /// Paths touched by a debounced batch of file events.
    Incremental {
        paths: usize,
    },
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
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
    mut on_change: impl FnMut(Vec<PathBuf>, BatchKind),
) -> Result<(), WatchError> {
    let (tx, rx) = std::sync::mpsc::channel::<DebounceEventResult>();
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
        move |res| {
            let _ = tx.send(res);
        },
        NoCache::new(),
        NotifyConfig::default(),
    )?;

    let filter = crate::scanner_filter::IndexFilter::new(root, config)?;

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
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(events)) => {
                filter.clear_cache();
                let mut touched: Vec<PathBuf> = Vec::new();
                for ev in events {
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
                    for p in &ev.event.paths {
                        if keep_event_path(&filter, root, p) {
                            touched.push(p.clone());
                        }
                    }
                }
                touched.sort();
                touched.dedup();
                if touched.is_empty() {
                    continue;
                }
                debug!(n = touched.len(), "debounced batch");
                let n = touched.len();
                on_change(touched, BatchKind::Incremental { paths: n });
            }
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
        let report = crate::scanner::scan(
            root,
            &mut guard,
            &config,
            crate::scanner::ScanSource::WorkingTree,
            crate::scanner::EmbedMode::Inline,
        )?;
        on_batch(WatchBatch {
            kind: BatchKind::InitialScan,
            report: &report,
        });
    }
    info!("initial scan complete; entering watch mode");

    watch_paths(root, &config, shutdown, |touched, kind| {
        let mut guard = store.lock().expect("store poisoned");
        match crate::scanner::scan_paths(root, &mut guard, &config, &touched, crate::scanner::EmbedMode::Inline) {
            Ok(report) => {
                on_batch(WatchBatch { kind, report: &report });
            }
            Err(e) => warn!(error = %e, "scan_paths failed"),
        }
    })
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
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Block until the watcher is demonstrably live, then drain what arming produced.
    ///
    /// Registering a filesystem watch is asynchronous, so a test that just sleeps before its
    /// trigger races the registration whenever the machine is busy. That race is invisible in the
    /// negative tests below — an unarmed watcher reports nothing, which is exactly what they
    /// assert, so they pass without having tested anything. This writes `probe.rs` under `root`
    /// until the callback answers (proving the watch is live) and then drains pending batches, so
    /// a following assertion measures the filter rather than the startup window.
    fn arm_watcher(root: &Path, path_rx: &mpsc::Receiver<Vec<PathBuf>>) {
        let probe = root.join("probe.rs");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            assert!(std::time::Instant::now() < deadline, "watcher never armed within 30s");
            std::fs::write(&probe, b"fn probe() {}\n").expect("write probe file");
            match path_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(_) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("watcher thread died"),
            }
        }
        std::fs::remove_file(&probe).expect("remove probe file");
        // Drain the arming batches (and the probe's own removal) so they cannot be mistaken for
        // an emission the assertion under test is meant to rule out.
        while path_rx.recv_timeout(Duration::from_millis(300)).is_ok() {}
    }

    /// `watch_paths` should hand the callback the repo-relative path of a file
    /// that changes under the watched root, within a bounded window. This is the
    /// primitive the MCP serve watcher funnels into `scan_and_refresh`.
    #[test]
    fn should_emit_changed_path_when_file_is_modified() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize tempdir");
        let mut config = crate::config::default_for_root(&root);
        config.watch.debounce_ms = 50;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (path_tx, path_rx) = mpsc::channel::<Vec<PathBuf>>();

        let root_for_thread = root.clone();
        let handle = std::thread::spawn(move || {
            watch_paths(&root_for_thread, &config, shutdown_rx, |paths, kind| {
                assert!(matches!(kind, BatchKind::Incremental { .. }));
                let _ = path_tx.send(paths);
            })
        });

        let target = root.join("hello.rs");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let received = loop {
            assert!(
                std::time::Instant::now() < deadline,
                "watcher never reported hello.rs within 30s"
            );
            std::fs::write(&target, b"fn main() {}\n").expect("write file");
            match path_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(paths) => break paths,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("watcher thread died"),
            }
        };
        assert!(
            received.iter().any(|p| p.ends_with("hello.rs")),
            "expected hello.rs in {received:?}"
        );

        let _ = shutdown_tx.send(());
        let _ = handle.join();
    }

    #[test]
    fn shutdown_interrupts_sustained_event_batches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize tempdir");
        let mut config = crate::config::default_for_root(&root);
        config.watch.debounce_ms = 1;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (batch_tx, batch_rx) = mpsc::channel::<usize>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let pulse = root.join("pulse.rs");
        let pulse_for_thread = pulse.clone();
        let root_for_thread = root.clone();
        let handle = std::thread::spawn(move || {
            let mut generation = 0usize;
            let result = watch_paths(&root_for_thread, &config, shutdown_rx, |_paths, _kind| {
                generation += 1;
                let _ = batch_tx.send(generation);
                let _ = std::fs::write(&pulse_for_thread, format!("fn pulse_{generation}() {{}}\n"));
            });
            let _ = done_tx.send(());
            result
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while batch_rx.recv_timeout(Duration::from_millis(100)).is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "watcher never entered the event loop"
            );
            std::fs::write(&pulse, b"fn pulse_0() {}\n").expect("write initial pulse");
        }
        for _ in 0..3 {
            batch_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("sustained batch arrives");
        }

        shutdown_tx.send(()).expect("signal shutdown under load");
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("watcher must observe shutdown without waiting for a quiet receive timeout");
        handle.join().expect("join watcher").expect("watcher succeeds");
    }

    /// A rename must still surface the new path. Under `NoCache` the debouncer no longer stitches
    /// FileId-based rename events, so a rename degrades to remove-old + create-new — we assert the
    /// create half reaches the callback so the renamed file gets (re)indexed. Guards the cache swap
    /// in `watch_paths` (issue #43).
    #[test]
    fn should_emit_new_path_when_file_is_renamed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize tempdir");
        let mut config = crate::config::default_for_root(&root);
        config.watch.debounce_ms = 50;

        let original = root.join("before.rs");
        std::fs::write(&original, b"fn main() {}\n").expect("seed file");

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (path_tx, path_rx) = mpsc::channel::<Vec<PathBuf>>();

        let root_for_thread = root.clone();
        let handle = std::thread::spawn(move || {
            watch_paths(&root_for_thread, &config, shutdown_rx, |paths, kind| {
                assert!(matches!(kind, BatchKind::Incremental { .. }));
                let _ = path_tx.send(paths);
            })
        });

        // A rename is one-shot — once `before.rs` is gone the trigger cannot be retried — so
        // unlike the sibling modify test this cannot re-fire inside the polling loop. It must
        // know the watch is live BEFORE renaming. ~keep
        arm_watcher(&root, &path_rx);

        let renamed = root.join("after.rs");
        std::fs::rename(&original, &renamed).expect("rename file");

        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let saw_new_path = loop {
            assert!(
                std::time::Instant::now() < deadline,
                "watcher never reported after.rs within 30s"
            );
            match path_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(paths) if paths.iter().any(|p| p.ends_with("after.rs")) => break true,
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("watcher thread died"),
            }
        };
        assert!(saw_new_path, "expected after.rs to surface post-rename");

        let _ = shutdown_tx.send(());
        let _ = handle.join();
    }

    /// Changes inside `.basemind/` must never surface — the watcher would
    /// otherwise feed its own index writes back into a rescan loop.
    #[test]
    fn should_ignore_changes_under_basemind_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize tempdir");
        std::fs::create_dir_all(root.join(crate::config::BASEMIND_DIR)).expect("mkdir .basemind");
        let mut config = crate::config::default_for_root(&root);
        config.watch.debounce_ms = 50;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (path_tx, path_rx) = mpsc::channel::<Vec<PathBuf>>();

        let root_for_thread = root.clone();
        let handle = std::thread::spawn(move || {
            watch_paths(&root_for_thread, &config, shutdown_rx, |paths, _kind| {
                let _ = path_tx.send(paths);
            })
        });

        arm_watcher(&root, &path_rx);
        std::fs::write(root.join(crate::config::BASEMIND_DIR).join("noise.txt"), b"ignored\n")
            .expect("write basemind file");

        let result = path_rx.recv_timeout(Duration::from_millis(800));
        assert!(result.is_err(), "expected no emission, got {result:?}");

        let _ = shutdown_tx.send(());
        let _ = handle.join();
    }

    /// Writes under a *nested* child-repo `.basemind/` and under a gitignored path must not wake a
    /// rescan — this is the core of issue #33 (an umbrella repo's watcher must ignore a nested
    /// serve's index flushes, and gitignored churn generally).
    #[test]
    fn should_ignore_nested_basemind_and_gitignored_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize tempdir");
        std::fs::create_dir_all(root.join(".git")).expect("mkdir .git");
        std::fs::create_dir_all(root.join("child").join(crate::config::BASEMIND_DIR)).expect("mkdir child/.basemind");
        std::fs::write(root.join(".gitignore"), b"build/\n").expect("write .gitignore");
        std::fs::create_dir_all(root.join("build")).expect("mkdir build");
        let mut config = crate::config::default_for_root(&root);
        config.watch.debounce_ms = 50;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (path_tx, path_rx) = mpsc::channel::<Vec<PathBuf>>();

        let root_for_thread = root.clone();
        let handle = std::thread::spawn(move || {
            watch_paths(&root_for_thread, &config, shutdown_rx, |paths, _kind| {
                let _ = path_tx.send(paths);
            })
        });

        arm_watcher(&root, &path_rx);
        std::fs::write(
            root.join("child")
                .join(crate::config::BASEMIND_DIR)
                .join("index.msgpack"),
            b"\x00",
        )
        .expect("write nested basemind file");
        std::fs::write(root.join("build").join("out.o"), b"\x00").expect("write gitignored file");

        let result = path_rx.recv_timeout(Duration::from_millis(800));
        assert!(
            result.is_err(),
            "expected no emission for nested-.basemind / gitignored churn, got {result:?}"
        );

        let _ = shutdown_tx.send(());
        let _ = handle.join();
    }

    /// A permission-denied directory must not break the watcher's startup. This is the entire
    /// point of the PR: with non-fatal watch registration, an unreadable tree is skipped (with a
    /// warning) instead of aborting `watch_paths` with a `notify error: Permission denied`.
    ///
    /// The reproduction only bites when the test runs as a non-root user (as in CI); as root the
    /// `chmod 000` directory is still readable, so the test passes trivially locally but exercises
    /// the EACCES path in CI.
    #[cfg(target_os = "linux")]
    #[test]
    fn should_not_fail_startup_on_unreadable_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        let locked = root.join("locked");
        std::fs::create_dir_all(&locked).expect("mkdir locked");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
        }
        let mut config = crate::config::default_for_root(&root);
        config.watch.debounce_ms = 50;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<Result<(), WatchError>>();
        let root_for_thread = root.clone();
        let handle = std::thread::spawn(move || {
            let result = watch_paths(&root_for_thread, &config, shutdown_rx, |_paths, _kind| {});
            let _ = done_tx.send(result);
        });

        // Give the watcher a moment to arm (registering watches), then shut it down.
        std::thread::sleep(Duration::from_millis(500));
        let _ = shutdown_tx.send(());
        let result = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("watcher thread died");
        let _ = handle.join();
        // Restore read/exec so the TempDir drop can remove the tree.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700));
        }
        assert!(
            result.is_ok(),
            "watch_paths must not fail on an unreadable directory: {result:?}"
        );
    }
}
