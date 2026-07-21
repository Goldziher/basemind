use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

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
    debouncer.watch(root, RecursiveMode::Recursive)?;

    let filter = crate::scanner_filter::IndexFilter::new(root, config)?;

    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(events)) => {
                filter.clear_cache();
                let mut touched: Vec<PathBuf> = Vec::new();
                for ev in events {
                    if !is_relevant(&ev.event.kind) {
                        continue;
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
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if shutdown.try_recv().is_ok() {
                    info!("shutdown requested; exiting watcher");
                    return Ok(());
                }
            }
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

        // ~keep Let the watcher arm before the rename so the create half is observed.
        std::thread::sleep(Duration::from_millis(200));
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

        std::thread::sleep(Duration::from_millis(200));
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
    #[ignore = "timing-sensitive negative assertion; flaky on CI. Filter covered by scanner_filter tests"]
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

        std::thread::sleep(Duration::from_millis(500));
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
}
