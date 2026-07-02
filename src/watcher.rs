use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, new_debouncer};
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
    let mut debouncer = new_debouncer(debounce, None, move |res| {
        let _ = tx.send(res);
    })?;
    debouncer.watch(root, RecursiveMode::Recursive)?;

    // Drop, at the watcher, every event the scanner would never index — so churn under
    // `node_modules`, `target`, `.git`, gitignored paths, and (crucially) *nested* child-repo
    // `.basemind/` flushes never wake `scan_and_refresh`/`scan_paths`. This subsumes the old two
    // `.basemind` `starts_with` guards: the default `**/.basemind/**` exclude drops the root AND
    // every nested `.basemind/`, and the empty/ancestor rel that macOS FSEvents coalesces onto the
    // watched root fails the glob gate too. On macOS FSEvents is kernel-recursive, so we cannot
    // un-watch excluded subtrees — but filtering here means a coalesced churn batch costs a few
    // microsecond path checks instead of a full-corpus MapCache rebuild (issue #33).
    let filter = crate::scanner_filter::IndexFilter::new(root, config)?;

    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(events)) => {
                // Fresh directory listings per batch so a newly-added or edited `.gitignore` takes
                // effect immediately.
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
        )?;
        on_batch(WatchBatch {
            kind: BatchKind::InitialScan,
            report: &report,
        });
    }
    info!("initial scan complete; entering watch mode");

    watch_paths(root, &config, shutdown, |touched, kind| {
        let mut guard = store.lock().expect("store poisoned");
        match crate::scanner::scan_paths(root, &mut guard, &config, &touched) {
            Ok(report) => {
                on_batch(WatchBatch {
                    kind,
                    report: &report,
                });
            }
            Err(e) => warn!(error = %e, "scan_paths failed"),
        }
    })
}

fn is_relevant(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
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
    // Drop any `.basemind/` directory entry or its contents at ANY nesting level: a nested child
    // repo's `.basemind/` flush (the issue #33 loop) AND the `.basemind` dir entry itself, which
    // the exclude glob (`**/.basemind/**`, matching only the *contents*) does not cover — FSEvents
    // reports the directory when its contents change, which would otherwise wake a rescan.
    if rel
        .components()
        .any(|c| c.as_os_str() == crate::config::BASEMIND_DIR)
    {
        return false;
    }
    let rel = rel.to_string_lossy().replace('\\', "/");
    if rel.is_empty() {
        return false;
    }
    if p.exists() {
        filter.is_indexable(p)
    } else {
        filter.allows_glob(&rel)
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
        // Canonicalize so the watched root matches the canonical paths notify
        // reports (on macOS /var is a symlink to /private/var), mirroring how
        // `main.rs` canonicalizes the root before handing it to the watcher.
        let root = tmp.path().canonicalize().expect("canonicalize tempdir");
        // Short debounce keeps the test fast.
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

        // Give the debouncer a moment to arm before mutating the tree. The
        // macOS FSEvents backend in particular needs the recursive watch fully
        // established before it will report subsequent writes.
        std::thread::sleep(Duration::from_millis(500));
        let target = root.join("hello.rs");
        std::fs::write(&target, b"fn main() {}\n").expect("write file");

        // Wait for the callback to surface the change. Generous window: the
        // backend latency dominates, not the debounce.
        let received = path_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("changed path within window");
        assert!(
            received.iter().any(|p| p.ends_with("hello.rs")),
            "expected hello.rs in {received:?}"
        );

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
        std::fs::write(
            root.join(crate::config::BASEMIND_DIR).join("noise.txt"),
            b"ignored\n",
        )
        .expect("write basemind file");

        // No callback should fire for a `.basemind/`-only change.
        let result = path_rx.recv_timeout(Duration::from_millis(800));
        assert!(result.is_err(), "expected no emission, got {result:?}");

        let _ = shutdown_tx.send(());
        let _ = handle.join();
    }

    /// Writes under a *nested* child-repo `.basemind/` and under a gitignored path must not wake a
    /// rescan — this is the core of issue #33 (an umbrella repo's watcher must ignore a nested
    /// serve's index flushes, and gitignored churn generally).
    // Gated off Windows: `notify` delivers directory-granular events there (via
    // ReadDirectoryChangesW), so a debounced batch can surface a parent directory (`child`,
    // `build`) that is not itself a `.basemind`/gitignored path — making the exact "no emission"
    // assertion below flaky on Windows even though the per-path filter is correct. The filtering
    // logic itself (`keep_event_path` + `IndexFilter`) is covered platform-agnostically by the
    // `scanner_filter` unit tests; unix (FSEvents/inotify) reports the file path, which the filter
    // drops as intended.
    #[cfg_attr(windows, ignore = "notify emits directory-granular events on Windows")]
    #[test]
    fn should_ignore_nested_basemind_and_gitignored_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize tempdir");
        // `.git` so the `ignore` crate honors `.gitignore` (it only applies git rules inside a repo).
        std::fs::create_dir_all(root.join(".git")).expect("mkdir .git");
        std::fs::create_dir_all(root.join("child").join(crate::config::BASEMIND_DIR))
            .expect("mkdir child/.basemind");
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
        // A nested child serve's index flush + a gitignored build artifact — neither is indexable.
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
