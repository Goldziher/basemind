//! A workspace's writes: the in-process rescan into its own store, publishing a store another
//! process wrote, the deferred map warm and the blob sweep. Each runs on the calling thread; the
//! host decides which thread that is (the MCP adapter spawns them, the CLI and an embedder call them).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::engine::MapCache;
use crate::engine::error::EngineError;
use crate::engine::l1_cache::budget_bytes_from;
use crate::engine::workspace::Workspace;
use crate::path::{RelPath, is_external_key, normalize_query_path};
use crate::scanner::{EmbedMode, FileResult, FileStatus, ScanError, ScanObserver, ScanReport};
use crate::store::Store;

/// The only thing a rescan wants out of a scan's per-file outcomes: which paths to fold into
/// the cache delta, and which to drop from it.
///
/// A [`ScanObserver`] rather than a filter over a collected `Vec`, because the two projections here
/// are all that ever needed to survive a file's result — the status, the buffered index entry and
/// the pending batches never leave the drive chunk.
#[derive(Default)]
struct RescanPaths {
    updated: Vec<RelPath>,
    removed: Vec<RelPath>,
}

impl ScanObserver for RescanPaths {
    fn on_file(&mut self, result: FileResult) {
        match result.status {
            FileStatus::Updated { .. } => self.updated.push(RelPath::from(result.path.as_str())),
            FileStatus::Removed => self.removed.push(RelPath::from(result.path.as_str())),
            _ => {}
        }
    }
}

/// The error a refused or failed rescan answers with. Another process writing this workspace is the
/// caller's to retry or rescan through: [`EngineError::Busy`], which the CLI exits `3` for as it does
/// for every other lock contention. A store that cannot be read names its repair
/// (`store_error` in `src/engine/error.rs`).
pub(crate) fn rescan_error(error: &ScanError) -> EngineError {
    match error {
        ScanError::Store(store) => super::error::store_error(store).in_context("rescan"),
        _ => EngineError::internal(format!("rescan: {error}")),
    }
}

impl Workspace {
    /// Run the scanner into this workspace's own store and refresh the in-RAM map, returning the
    /// raw [`ScanReport`]. The `index_rescan` operation, the startup auto-scan and the serve watcher
    /// all go through this one scan + map-swap path against the workspace's own `Store`, so none of
    /// them releases or collides with the index lock the process already holds.
    ///
    /// Holds the store write lock for the duration of the scan; reads block while it runs
    /// (acceptable for small/medium repos — a rescan is agent-triggered, not per-request).
    /// `scoped_paths` limits the scan to those absolute paths under the root; `None` runs a full
    /// working-tree scan. `named` says the paths came from a person or an agent, not from the
    /// watcher: a named file that reads as unchanged has its blob validated.
    pub(crate) fn rescan(
        &self,
        scoped_paths: Option<Vec<PathBuf>>,
        embed: EmbedMode,
        named: bool,
    ) -> Result<ScanReport, EngineError> {
        if self.read_only {
            return Err(EngineError::unavailable(
                "this basemind serve is read-only: another serve process holds the write lock for \
                 this repo, so it owns index refresh. Reads are served from the shared index; run \
                 rescans from the lock-holding serve (or close it and retry).",
            ));
        }
        // The last in-process full-tree walk that the CLI's own guard does not cover (issue #62). The
        // tool-subcommand dispatcher never calls `guard_workspace_root`, so the `index_rescan` tool and
        // the boot/watcher scans reach `crate::scanner::scan` here. Guarding at this single funnel
        // covers all of them; it does NOT cover the read-only store open that
        // `cli::context::build_server` already performed, so a refused root can still have had its
        // cache directory created by that point.
        //
        // Used as a pure admission check: the scan keeps running against `self.root`, not the
        // resolved path. This workspace's store, `MapCache` and any scoped paths were all built from
        // that exact spelling, so swapping in a differently-spelled-but-equal root here would make
        // `strip_prefix` miss every scoped path. Resolution belongs where the root is first chosen.
        crate::config::root_guard::workspace_root_verdict(&self.root).map_err(|refusal| {
            EngineError::unavailable(crate::config::root_guard::refusal_message(&self.root, refusal))
        })?;

        let was_scoped = scoped_paths.is_some();
        let (report, paths) = self
            .scan_into_store(scoped_paths, embed, named)
            .map_err(|e| rescan_error(&e))?;

        if was_scoped && report.stats.updated == 0 && report.stats.removed == 0 {
            return Ok(report);
        }

        let RescanPaths { updated, removed } = paths;
        let new_cache = {
            let store = self.store_read();
            let corpus_bytes: u64 = store.index.files.values().map(|e| e.size_bytes).sum();
            self.corpus_bytes.store(corpus_bytes, Ordering::Relaxed);
            let current = self.cache.load();
            if was_scoped {
                Some(current.with_delta(&store, &updated, &removed))
            } else {
                current.refreshed(&store, budget_bytes_from(&self.config.resources))
            }
        };
        if let Some(cache) = new_cache {
            self.cache.store(Arc::new(cache));
        }
        self.note_index_changed();

        Ok(report)
    }

    /// The scan itself, under the store write lock.
    fn scan_into_store(
        &self,
        scoped_paths: Option<Vec<PathBuf>>,
        embed: EmbedMode,
        named: bool,
    ) -> Result<(ScanReport, RescanPaths), ScanError> {
        let mut store = self.store_write();
        // The store may have been opened via `Store::open_read_only` (the CLI / MCP query path),
        // which never creates `view_dir` on disk. On a workspace's first-ever scan that directory
        // is still missing here, and `flush()` would fail with ENOENT writing `index.msgpack.tmp`.
        // ~keep
        store.ensure_dirs()?;
        let mut observer = RescanPaths::default();
        let cancel = crate::scanner::ScanCancel::new();
        // A full Inline pass loads the embedding engine; release it once the pass is over (incremental
        // watcher batches keep it warm for the searches that follow an edit).
        #[cfg(feature = "intelligence")]
        let _embed_pass =
            (matches!(embed, EmbedMode::Inline) && scoped_paths.is_none()).then(crate::embeddings::begin_embed_pass);
        let (root, config) = (&self.root, &self.config);
        let report = if let Some(paths) = scoped_paths {
            let scan = if named {
                crate::scanner::scan_named_paths_with_observer
            } else {
                crate::scanner::scan_paths_with_observer
            };
            scan(
                root,
                &mut store,
                config,
                &paths,
                embed,
                &cancel,
                &mut observer,
                crate::retrieval_wiring::backend(),
            )
        } else {
            crate::scanner::scan_with_observer(
                root,
                &mut store,
                config,
                crate::scanner::ScanSource::WorkingTree,
                embed,
                &cancel,
                &mut observer,
                crate::retrieval_wiring::backend(),
            )
        };
        report.map(|report| (report, observer))
    }

    /// Serve `store`, which another process (the daemon's pool writer) just rewrote the file map of,
    /// and derive the in-RAM map from the current one ([`MapCache::refreshed`]: only the paths the
    /// scan changed are re-projected, and unchanged outlines stay decoded). Returns the map when one
    /// was published, so the caller can warm its term index.
    ///
    /// Swaps BOTH the store and the map. The writer just rewrote `index.msgpack`, so this workspace's
    /// in-memory [`crate::store::Index`] is stale: map-reading operations would pick up a new map, but
    /// store-reading ones (`index_status`'s `file_count`, the corpus size) read `store.index` directly
    /// and would report the pre-scan (often empty) index forever.
    #[cfg_attr(
        not(all(feature = "comms", any(unix, windows))),
        allow(
            dead_code,
            reason = "only a daemon-hosted workspace serves a store another process wrote"
        )
    )]
    pub(crate) fn publish_store(&self, store: Store) -> Option<Arc<MapCache>> {
        let current = self.cache.load_full();
        let cache = current.refreshed(&store, budget_bytes_from(&self.config.resources));
        *self.store_write() = store;
        let published = cache.map(|cache| {
            let cache = Arc::new(cache);
            self.cache.store(Arc::clone(&cache));
            cache
        });
        self.note_index_changed();
        published
    }

    /// [`publish_store`](Self::publish_store) only when `store` maps the workspace differently from
    /// the map served now; `None` when it does not, and nothing is replaced or bumped.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    pub(crate) fn publish_store_if_changed(&self, store: Store) -> Option<Arc<MapCache>> {
        let cache = Arc::new(
            self.cache
                .load_full()
                .refreshed(&store, budget_bytes_from(&self.config.resources))?,
        );
        *self.store_write() = store;
        self.cache.store(Arc::clone(&cache));
        self.note_index_changed();
        Some(cache)
    }

    /// The deferred in-RAM map preload of a workspace opened with its map warming
    /// ([`Workspace::wait_for_warm`]): build the map from every indexed blob, publish it, open the
    /// warm gate every map-reading call waits on, then build the map's term index so the first
    /// `code_symbols` lookup after boot is as fast as every later one.
    ///
    /// The gate opens however the build ends: a panic in it leaves the placeholder map served and the
    /// waiters released, never blocked for the whole wait cap.
    pub(crate) fn warm(&self) {
        struct OpenGate<'a>(&'a Workspace);
        impl Drop for OpenGate<'_> {
            fn drop(&mut self) {
                self.0.finish_warm();
            }
        }
        let started = std::time::Instant::now();
        let gate = OpenGate(self);
        let cache = {
            let store = self.store_read();
            MapCache::build(&store, budget_bytes_from(&self.config.resources))
        };
        let files = cache.len();
        let cache = Arc::new(cache);
        self.cache.store(Arc::clone(&cache));
        self.cache_generation.fetch_add(1, Ordering::Relaxed);
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.cache_warm_ms.store(elapsed_ms, Ordering::Relaxed);
        drop(gate);
        tracing::info!(files, elapsed_ms, "in-RAM code map warm complete");
        cache.terms();
    }

    /// Sweep the machine-global blob store against this workspace's referenced blobs. `None` when
    /// the blob cache is shared across git worktrees, whose other checkouts this workspace cannot
    /// see the references of.
    ///
    /// Uses the UNLOCKED `store_gc` primitives (`collect_referenced_hashes` + `gc_blobs`) under the
    /// store read lock — NEVER `store_gc::run_gc`, which re-acquires the `.basemind/.lock` flock the
    /// process serving this workspace already holds (that would deadlock). The read lock blocks the
    /// only in-process writer ([`rescan`](Self::rescan)) for the mark and sweep.
    pub(crate) fn gc_blobs(&self) -> Result<Option<crate::store_gc::GcReport>, EngineError> {
        let store = self.store_read();
        if store.blobs_shared {
            return Ok(None);
        }
        let referenced = crate::store_gc::collect_referenced_hashes(&store.basemind_dir)
            .map_err(|error| EngineError::internal(error.to_string()))?;
        crate::store_gc::gc_blobs(&referenced)
            .map(Some)
            .map_err(|error| EngineError::internal(error.to_string()))
    }
}

/// Normalize and validate `raw` rescan targets against `root`, returning absolute paths.
///
/// A target that no longer exists on disk is accepted only when the index still knows it (or, for a
/// deleted directory, anything under it): rescanning a deleted file is how its entry is pruned,
/// whereas a typo should fail loudly instead of reporting a green no-op. Every problem is collected
/// into one message so the caller fixes them in a single pass.
pub(crate) fn resolve_rescan_paths(root: &Path, raw: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut resolved = Vec::with_capacity(raw.len());
    let mut problems = Vec::new();
    let mut indexed: Option<crate::store::Index> = None;
    for spelling in raw {
        let Some(rel) = normalize_query_path(spelling, root) else {
            problems.push(format!(
                "{spelling:?} is outside the repository root or names the root itself (pass no path to rescan \
                 everything)"
            ));
            continue;
        };
        let abs = root.join(&rel);
        // ~keep An absolute path outside the repo is only ever a `scan.extra_roots` file, which lives
        // ~keep in the index under its absolute key; anything else outside the root is not a target.
        let external = is_external_key(rel.as_bytes());
        if (external || !abs.exists()) && !is_indexed(root, &mut indexed, &rel) {
            problems.push(if external {
                format!("{spelling:?} is outside the repository and not an indexed extra root")
            } else {
                format!("{spelling:?} does not exist and is not in the index")
            });
            continue;
        }
        resolved.push(abs);
    }
    if problems.is_empty() {
        return Ok(resolved);
    }
    Err(format!("invalid rescan path(s):\n  {}", problems.join("\n  ")))
}

/// Whether the working-view index holds `rel` (a file) or anything beneath it (a directory).
/// Loaded once per call, only when a path is missing from disk.
fn is_indexed(root: &Path, cache: &mut Option<crate::store::Index>, rel: &str) -> bool {
    if cache.is_none() {
        *cache = Store::open_read_only_no_index(root, crate::store::VIEW_WORKING)
            .ok()
            .map(|store| store.index);
    }
    let Some(index) = cache else {
        return false;
    };
    if index.files.contains_key(&RelPath::from(rel)) {
        return true;
    }
    let prefix = format!("{rel}/");
    index
        .files
        .keys()
        .any(|key| key.as_str().is_some_and(|key| key.starts_with(&prefix)))
}
