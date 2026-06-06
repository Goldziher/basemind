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

/// Run the watcher loop. Blocks until the shutdown receiver fires or the debouncer
/// channel disconnects. Performs an initial full scan, then listens for file events
/// and re-scans only the touched paths via `scanner::scan_paths`.
pub fn watch(
    root: &Path,
    store: Arc<Mutex<Store>>,
    config: Arc<Config>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
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

    let (tx, rx) = std::sync::mpsc::channel::<DebounceEventResult>();
    let debounce = Duration::from_millis(config.watch.debounce_ms);
    let mut debouncer = new_debouncer(debounce, None, move |res| {
        let _ = tx.send(res);
    })?;
    debouncer.watch(root, RecursiveMode::Recursive)?;

    let gitmind_subpath = root.join(crate::config::GITMIND_DIR);

    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(events)) => {
                let mut touched: Vec<PathBuf> = Vec::new();
                for ev in events {
                    if !is_relevant(&ev.event.kind) {
                        continue;
                    }
                    for p in &ev.event.paths {
                        if p.starts_with(&gitmind_subpath) {
                            continue;
                        }
                        touched.push(p.clone());
                    }
                }
                touched.sort();
                touched.dedup();
                if touched.is_empty() {
                    continue;
                }
                debug!(n = touched.len(), "debounced batch");
                let mut guard = store.lock().expect("store poisoned");
                match crate::scanner::scan_paths(root, &mut guard, &config, &touched) {
                    Ok(report) => {
                        on_batch(WatchBatch {
                            kind: BatchKind::Incremental {
                                paths: touched.len(),
                            },
                            report: &report,
                        });
                    }
                    Err(e) => warn!(error = %e, "scan_paths failed"),
                }
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

fn is_relevant(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}
