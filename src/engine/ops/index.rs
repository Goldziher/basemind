//! `index_status` and `index_rescan`: the index's health and identity, and re-indexing it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use crate::engine::error::EngineError;
use crate::engine::timing::elapsed_us;
use crate::engine::types::files::SizedFile;
use crate::engine::types::index::{IndexRescanParams, IndexStatusParams, LargeFiles, RescanResponse, StatusResponse};
use crate::engine::workspace::Workspace;
use crate::scanner::{EmbedMode, ScanStats};

/// The counts a [`RescanResponse`] reports, from a scan into the workspace's own store or one a
/// writer in another process ran for it (which reports no per-file skip breakdown, so those read 0).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct RescanStats {
    pub(crate) scanned: usize,
    pub(crate) updated: usize,
    pub(crate) docs_indexed: usize,
    pub(crate) docs_degraded: usize,
    pub(crate) docs_text_fallback: usize,
    pub(crate) doc_timeouts: usize,
    pub(crate) removed: usize,
    pub(crate) skipped_unchanged: usize,
    pub(crate) skipped_no_lang: usize,
    pub(crate) extract_failed: usize,
}

impl From<&ScanStats> for RescanStats {
    fn from(stats: &ScanStats) -> Self {
        Self {
            scanned: stats.scanned,
            updated: stats.updated,
            docs_indexed: stats.docs_indexed,
            docs_degraded: stats.docs_degraded,
            docs_text_fallback: stats.docs_text_fallback,
            doc_timeouts: stats.doc_timeouts,
            removed: stats.removed,
            skipped_unchanged: stats.skipped_unchanged,
            skipped_no_lang: stats.skipped_no_lang,
            extract_failed: stats.extract_failed,
        }
    }
}

/// Lines from which `index_status` counts a code file as too long to read whole: past a few
/// thousand lines one read spends most of an agent's context, so `code_outline` and a line range
/// serve better (#127).
pub(crate) const LARGE_FILE_LINES: u32 = 2000;

/// Large files `index_status` names; `find_files` with `min_lines` lists the rest.
const LARGE_FILES_LISTED: usize = 5;

/// Most-imported files `index_status` names; `basemind overview` lists more.
const MOST_IMPORTED_LISTED: usize = 5;

/// The code files of at least [`LARGE_FILE_LINES`] lines: their count and the
/// [`LARGE_FILES_LISTED`] longest, largest first and ties by path.
fn large_files<'a>(
    files: impl IntoIterator<Item = (&'a crate::path::RelPath, &'a crate::store::FileEntry)>,
) -> LargeFiles {
    let mut large: Vec<(&crate::path::RelPath, &crate::store::FileEntry)> = files
        .into_iter()
        .filter(|(_, entry)| entry.lines >= LARGE_FILE_LINES)
        .collect();
    let count = large.len();
    large.sort_unstable_by(|a, b| b.1.lines.cmp(&a.1.lines).then_with(|| a.0.cmp(b.0)));
    let largest = large
        .into_iter()
        .take(LARGE_FILES_LISTED)
        .map(|(path, entry)| SizedFile {
            path: path.clone(),
            lines: entry.lines,
            bytes: entry.size_bytes,
        })
        .collect();
    LargeFiles {
        threshold_lines: LARGE_FILE_LINES,
        count,
        largest,
    }
}

/// Where `index_rescan` reports progress, for a host that shows it (an MCP progress token).
pub(crate) trait RescanProgress {
    /// The targets were accepted and the scan starts.
    fn started(&mut self) {}
    /// The scan finished, having looked at `scanned` files.
    fn finished(&mut self, _scanned: usize) {}
}

impl Workspace {
    /// `index_status`: index health for the workspace's view — file counts, per-language breakdown,
    /// on-disk blob count, and the lifecycle flags a client needs to tell "still indexing" apart from
    /// "nothing indexed" — plus repository identity.
    ///
    /// A store held by a writer is a report, not an error: another task or process writing the index
    /// means a rebuild is in flight, which is exactly what `rebuild_in_progress` tells the caller.
    pub(crate) fn index_status(&self, _p: IndexStatusParams) -> Result<StatusResponse, EngineError> {
        let body = std::time::Instant::now();
        let queued_scan = self.scan_queue.queued();
        let indexing = self.initial_scan_active.load(Ordering::Relaxed);
        let index_build_ms = {
            let ms = self.initial_scan_ms.load(Ordering::Relaxed);
            (ms > 0).then_some(ms)
        };
        let warming = self.cache_warming.load(Ordering::Relaxed);
        let history_syncing = self.history_syncing.load(Ordering::Relaxed);
        let warm_ms = {
            let ms = self.cache_warm_ms.load(Ordering::Relaxed);
            (ms > 0).then_some(ms)
        };
        let notice = self.lifecycle_notice();
        let identity = self.repo_identity();
        let submodules = self.repo.as_ref().map(|r| r.submodule_paths()).unwrap_or_default();
        let Ok(store) = self.try_store_read() else {
            return Ok(StatusResponse {
                file_count: 0,
                blob_count: count_fm_blobs(),
                note: Some(
                    "a rebuild is in progress (another basemind process holds the store \
                     lock); index counts are unavailable until it completes"
                        .to_string(),
                ),
                rescan_needed: false,
                rebuild_in_progress: true,
                partial: false,
                queued_scan,
                degraded: None,
                indexing,
                index_build_ms,
                warming,
                warm_ms,
                history_syncing,
                notice,
                total_size_bytes: 0,
                languages: BTreeMap::new(),
                large_files: None,
                most_imported: Vec::new(),
                cache_dir: self.cache_dir(),
                grammar_dir: grammar_dir(),
                schema_version: crate::extract::SCHEMA_VER,
                root: self.root.display().to_string(),
                submodules,
                deps_unavailable: Vec::new(),
                config_stamp: self.config_stamp(),
                branch: identity.branch,
                head_sha: identity.head_sha,
                head_short_sha: identity.head_short_sha,
                elapsed_us: elapsed_us(body),
            });
        };
        let mut by_lang_ref: BTreeMap<&str, usize> = BTreeMap::new();
        let mut total_size: u64 = 0;
        for entry in store.index.files.values() {
            *by_lang_ref.entry(entry.language.as_str()).or_insert(0) += 1;
            total_size = total_size.saturating_add(entry.size_bytes);
        }
        let large_files = large_files(&store.index.files);
        // No graph (no index view in this session) is no list, not a failed status.
        let most_imported = crate::engine::import_graph::import_graph_held(self, &store)
            .map(|graph| crate::engine::ops::overview::most_imported(&graph, MOST_IMPORTED_LISTED).0)
            .unwrap_or_default();
        let by_lang: BTreeMap<String, usize> = by_lang_ref.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        let file_count = store.index.files.len();
        let blob_count = count_fm_blobs();
        let rescan_needed = store.rescan_owed();
        let note = index_note(rescan_needed, file_count, blob_count);
        let last_scan = crate::store::read_status_sidecar(&store.basemind_dir);
        Ok(StatusResponse {
            file_count,
            blob_count,
            note,
            rescan_needed,
            rebuild_in_progress: false,
            partial: last_scan.as_ref().is_some_and(|status| status.partial),
            queued_scan,
            degraded: last_scan
                .as_ref()
                .and_then(crate::store::StatusSidecar::degraded_summary),
            indexing,
            index_build_ms,
            warming,
            warm_ms,
            history_syncing,
            notice,
            total_size_bytes: total_size,
            languages: by_lang,
            large_files: Some(large_files),
            most_imported,
            cache_dir: self.cache_dir(),
            grammar_dir: grammar_dir(),
            schema_version: crate::extract::SCHEMA_VER,
            root: self.root.display().to_string(),
            submodules,
            deps_unavailable: crate::deps_docs::read_unavailable(&store.basemind_dir),
            config_stamp: self.config_stamp(),
            branch: identity.branch,
            head_sha: identity.head_sha,
            head_short_sha: identity.head_short_sha,
            elapsed_us: elapsed_us(body),
        })
    }

    /// `index_rescan`: re-index `paths`, or the whole working tree when none is given, reporting to
    /// `progress`. `scan` runs the scan of the resolved targets (`None` for the whole working tree)
    /// wherever this workspace's writer is: [`rescan_own_store`](Self::rescan_own_store) when this
    /// process holds it.
    pub(crate) fn index_rescan(
        &self,
        p: IndexRescanParams,
        progress: &mut dyn RescanProgress,
        scan: impl FnOnce(&Self, Option<Vec<PathBuf>>) -> Result<RescanStats, EngineError>,
    ) -> Result<RescanResponse, EngineError> {
        let started = std::time::Instant::now();
        let scoped_paths = match p.paths {
            None => None,
            Some(requested) => Some(crate::engine::scan::resolve_rescan_paths(&self.root, &requested).map_err(
                |e| {
                    EngineError::invalid_argument(format!(
                        "`index_rescan`: {e}\nName files or directories inside the repository; locate one with `find_files`."
                    ))
                },
            )?),
        };
        let root = self.root.display().to_string();
        progress.started();
        let stats = scan(self, scoped_paths)?;
        progress.finished(stats.scanned);
        Ok(RescanResponse {
            scanned: stats.scanned,
            updated: stats.updated,
            docs_indexed: stats.docs_indexed,
            docs_degraded: stats.docs_degraded,
            docs_text_fallback: stats.docs_text_fallback,
            doc_timeouts: stats.doc_timeouts,
            removed: stats.removed,
            skipped_unchanged: stats.skipped_unchanged,
            skipped_no_lang: stats.skipped_no_lang,
            extract_failed: stats.extract_failed,
            elapsed_ms: started.elapsed().as_millis(),
            root,
        })
    }

    /// The scan of [`index_rescan`](Self::index_rescan) in this workspace's own store: an embedding
    /// pass, with the named targets' blobs validated.
    pub(crate) fn rescan_own_store(&self, scoped_paths: Option<Vec<PathBuf>>) -> Result<RescanStats, EngineError> {
        let report = self.rescan(scoped_paths, EmbedMode::Inline, true)?;
        Ok(RescanStats::from(&report.stats))
    }

    /// Where basemind keeps this workspace's index, views and caches.
    fn cache_dir(&self) -> String {
        crate::store::workspace_cache_dir(&self.root).display().to_string()
    }

    fn config_stamp(&self) -> String {
        crate::config::daemon::ConfigStamp::of(&self.root).to_string()
    }

    fn repo_identity(&self) -> RepoIdentity {
        let info = self.repo.as_ref().and_then(|repo| repo.info().ok());
        RepoIdentity {
            branch: info.as_ref().and_then(|info| info.branch.clone()),
            head_sha: info.as_ref().and_then(|info| info.head_sha.clone()),
            head_short_sha: info.and_then(|info| info.head_short_sha),
        }
    }
}

/// Branch and HEAD of the served repository; all `None` outside a git repository.
struct RepoIdentity {
    branch: Option<String>,
    head_sha: Option<String>,
    head_short_sha: Option<String>,
}

/// Where the tree-sitter grammars are cached; shared with every other user of the language pack.
fn grammar_dir() -> Option<String> {
    crate::lang::grammar_cache_dir().map(|dir| dir.display().to_string())
}

/// Count content-addressed blobs in the GLOBAL blob store by tallying `.fm.msgpack` files
/// (one combined L1 + L2 filemap per indexed content hash; the `.doc` sibling shares the same
/// stem so it is not double-counted). A single directory read — cheaper than
/// [`crate::store_gc::cache_stats`], which also unions every view index — so it is safe to call
/// from the `status` path.
///
/// Since the blob store went machine-global this counts blobs across EVERY workspace, so the
/// `status` divergence heuristic (empty index yet blobs on disk) can over-report on a shared
/// machine — advisory only, as before.
///
/// Returns `0` when the blobs directory is absent or unreadable; the count is advisory.
fn count_fm_blobs() -> usize {
    crate::store::count_fm_blobs(&crate::store::global_blobs_dir())
}

/// The `status` note: that a rescan is owed (see [`crate::store::Store::rescan_owed`]), else the
/// divergence note below.
///
/// The divergence note (bug #10): when the current view's index is empty
/// but content-addressed blobs are present on disk, the index was lost/wiped over live
/// blobs and a rescan would rebuild it. A legitimately unscanned repo (no blobs either)
/// gets no note.
fn index_note(rescan_owed: bool, file_count: usize, blob_count: usize) -> Option<String> {
    if rescan_owed {
        return Some(crate::store::RESCAN_OWED.to_string());
    }
    blob_divergence_note(file_count, blob_count)
}

fn blob_divergence_note(file_count: usize, blob_count: usize) -> Option<String> {
    if file_count == 0 && blob_count > 0 {
        Some(format!(
            "index for this view is empty but {blob_count} blob file(s) exist on disk; \
             the view index was lost or wiped — run `basemind scan` to rebuild it"
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_note_absent_when_index_and_blobs_agree() {
        assert_eq!(blob_divergence_note(42, 100), None, "populated index: no note");
    }

    #[test]
    fn status_note_absent_for_unscanned_empty_repo() {
        assert_eq!(
            blob_divergence_note(0, 0),
            None,
            "empty index with no blobs is a legitimately unscanned repo, not a lost index"
        );
    }

    #[test]
    fn status_note_says_a_rescan_is_owed_before_anything_else() {
        let note = index_note(true, 0, 7).expect("an owed rescan always has a note");
        assert!(
            note.contains("basemind scan") && note.contains("index_rescan"),
            "{note}"
        );
        assert_eq!(index_note(false, 42, 100), None);
        assert!(index_note(false, 0, 7).is_some_and(|note| note.contains("7 blob file")));
    }

    #[test]
    fn status_note_present_when_index_empty_but_blobs_exist() {
        let note = blob_divergence_note(0, 7);
        assert!(
            note.is_some(),
            "lost-index-over-live-blobs must surface a note (bug #10)"
        );
        let note = note.unwrap();
        assert!(note.contains("7 blob file"), "note reports the blob count");
        assert!(note.contains("scan"), "note suggests a rescan");
    }

    fn entry(lines: u32, bytes: u64) -> crate::store::FileEntry {
        crate::store::FileEntry {
            hash_hex: String::new(),
            language: "rust".to_string(),
            size_bytes: bytes,
            mtime: 0,
            extract_epoch: 0,
            content_key: None,
            lines,
        }
    }

    #[test]
    fn large_files_counts_from_the_threshold_and_names_the_longest_first() {
        let sizes = [
            ("a.rs", 1999),
            ("b.rs", 2000),
            ("c.rs", 9000),
            ("d.rs", 3000),
            ("e.rs", 3000),
            ("f.rs", 2500),
            ("g.rs", 2100),
            ("h.rs", 2050),
        ];
        let files: Vec<(crate::path::RelPath, crate::store::FileEntry)> = sizes
            .iter()
            .map(|(path, lines)| (crate::path::RelPath::from(*path), entry(*lines, u64::from(*lines) * 10)))
            .collect();
        let large = large_files(files.iter().map(|(path, entry)| (path, entry)));
        assert_eq!(large.threshold_lines, 2000);
        assert_eq!(large.count, 7, "1999 lines is under the threshold, 2000 is at it");
        let named: Vec<(String, u32, u64)> = large
            .largest
            .iter()
            .map(|file| (file.path.to_string(), file.lines, file.bytes))
            .collect();
        assert_eq!(
            named,
            [
                ("c.rs".to_string(), 9000, 90_000),
                ("d.rs".to_string(), 3000, 30_000),
                ("e.rs".to_string(), 3000, 30_000),
                ("f.rs".to_string(), 2500, 25_000),
                ("g.rs".to_string(), 2100, 21_000),
            ]
        );
    }
}
