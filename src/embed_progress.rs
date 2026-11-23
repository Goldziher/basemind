//! Progress of the background embedding pass: the record that lets `index_status` report
//! `embeddings {done, total, eta_secs}` and a restarted process resume where a killed one stopped.
//!
//! A fast (`Deferred`) scan indexes every document's text and leaves its vectors owed; this module
//! keeps one small record, `embed-progress.json` in the workspace cache dir, for the pass that
//! pays them. It is written when a `Deferred` pass leaves vectors owed, rewritten once per drive
//! chunk by the `Inline` pass that embeds them, and removed when that pass completes. A record
//! that outlives its process is the durable mark of an interrupted embedding pass: the record
//! carries the writer's identity, so a later process tells a live writer from a dead one exactly as
//! [`crate::scan_evidence`] does for the scan breadcrumb.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

/// Filename of the record inside a workspace cache dir.
const PROGRESS_FILE: &str = "embed-progress.json";

/// The record an embedding pass leaves in the workspace cache dir.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedRecord {
    /// Documents owed their vectors when the pass began (grown to `done` if the pass embedded more).
    pub total: usize,
    /// Documents embedded so far by the pass.
    pub done: usize,
    /// Unix milliseconds at which an `Inline` pass began embedding; absent while the vectors are
    /// owed and no pass has started.
    #[serde(default)]
    pub started_unix_ms: Option<u64>,
    /// The pid of the process that wrote the record last. The liveness test that separates a pass
    /// in progress from one that was killed.
    pub pid: u32,
    /// The writer's `crate::boot_identity::process_start`; tells it from a later holder of its pid.
    #[serde(default)]
    pub process_start: Option<u64>,
    /// The boot the writer ran in (`crate::boot_identity::current`).
    #[serde(default)]
    pub boot: Option<String>,
}

/// The `embeddings` block of `index_status`: how far the background embedding pass has got.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct EmbeddingsStatus {
    /// Documents whose vectors have been written.
    pub done: usize,
    /// Documents owed vectors by the scan, including those done.
    pub total: usize,
    /// Seconds until the pass finishes at the rate it has run so far. Absent while no pass runs
    /// (interrupted, or not yet started) and before the pass has embedded its first document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<u64>,
}

impl EmbeddingsStatus {
    /// What a semantic search says while vectors are still owed: the documents without them are
    /// found by their text alone.
    pub fn notice(&self) -> String {
        format!(
            "embeddings are pending ({} of {} documents embedded): documents without vectors are matched by \
             keyword only until the background embedding finishes; see `embeddings` in `index_status`",
            self.done, self.total
        )
    }
}

/// Path of the record for a workspace cache dir.
pub fn progress_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join(PROGRESS_FILE)
}

/// Read the record in a workspace cache dir. `None` when there is none or it is unreadable.
pub fn read(cache_dir: &Path) -> Option<EmbedRecord> {
    let bytes = std::fs::read(progress_path(cache_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Remove the record: the vectors it counted are written, or no pass owes any.
pub fn clear(cache_dir: &Path) {
    let _ = std::fs::remove_file(progress_path(cache_dir));
}

/// Write `record` via a sibling temp file plus a rename, so a reader never sees a half-written
/// record. Best-effort: a scan never fails because its progress could not be written.
fn write(cache_dir: &Path, record: &EmbedRecord) {
    let Ok(bytes) = serde_json::to_vec(record) else {
        return;
    };
    let path = progress_path(cache_dir);
    let temp = path.with_extension("json.tmp");
    if std::fs::create_dir_all(cache_dir).is_ok()
        && std::fs::write(&temp, bytes).is_ok()
        && std::fs::rename(&temp, &path).is_err()
    {
        let _ = std::fs::remove_file(&temp);
    }
}

/// A record owned by this process, `done` of `total` documents embedded.
fn record_of(total: usize, done: usize, started_unix_ms: Option<u64>) -> EmbedRecord {
    EmbedRecord {
        total,
        done,
        started_unix_ms,
        pid: std::process::id(),
        process_start: crate::boot_identity::process_start(std::process::id()),
        boot: crate::boot_identity::current(),
    }
}

/// Note that a `Deferred` pass left `total` documents owed their vectors, with no pass embedding
/// them yet. A process that finds this record without a live writer resumes the embedding.
pub fn record_owed(cache_dir: &Path, total: usize) {
    write(cache_dir, &record_of(total, 0, None));
}

/// Reports an `Inline` pass's embedding progress, once per drive chunk. The count it reports is the
/// scan's own, read from `embedded` (bumped by the workers as each document gets its vectors).
pub(crate) struct EmbedReporter<'a> {
    cache_dir: PathBuf,
    total: usize,
    started_unix_ms: u64,
    embedded: &'a AtomicUsize,
}

impl<'a> EmbedReporter<'a> {
    /// Start reporting a pass that embeds `total` documents, writing the first record at once.
    #[cfg(any(feature = "documents", test))]
    pub(crate) fn begin(cache_dir: PathBuf, total: usize, embedded: &'a AtomicUsize) -> Self {
        let reporter = Self {
            cache_dir,
            total,
            started_unix_ms: now_unix_ms(),
            embedded,
        };
        reporter.write(0);
        reporter
    }

    /// Record how many documents the pass has embedded so far.
    pub(crate) fn report(&self) {
        self.write(self.embedded.load(Ordering::Relaxed));
    }

    fn write(&self, done: usize) {
        write(
            &self.cache_dir,
            &record_of(self.total.max(done), done, Some(self.started_unix_ms)),
        );
    }
}

/// The pass's progress at `now_unix_ms`, or `None` when no embedding is owed.
pub fn status(cache_dir: &Path, now_unix_ms: u64) -> Option<EmbeddingsStatus> {
    let record = read(cache_dir)?;
    let total = record.total.max(record.done);
    if total == 0 {
        return None;
    }
    let eta_secs = record
        .started_unix_ms
        .filter(|_| writer_is_running(&record))
        .and_then(|started| eta_from_rate(now_unix_ms.saturating_sub(started), record.done, total));
    Some(EmbeddingsStatus {
        done: record.done,
        total,
        eta_secs,
    })
}

/// Whether the embedding was interrupted: a record exists and nothing is writing it. The condition
/// under which a booting process starts the pass again.
pub fn resume_due(cache_dir: &Path) -> bool {
    read(cache_dir).is_some_and(|record| !writer_is_running(&record))
}

/// Seconds left for the `total - done` documents still to go, at the rate `done` took `elapsed_ms`,
/// rounded up. `None` when no rate is known yet or nothing is left.
fn eta_from_rate(elapsed_ms: u64, done: usize, total: usize) -> Option<u64> {
    if done == 0 || done >= total || elapsed_ms == 0 {
        return None;
    }
    let remaining = u128::from(elapsed_ms) * (total - done) as u128;
    u64::try_from(remaining.div_ceil(done as u128 * 1000)).ok()
}

#[cfg(any(unix, windows))]
fn writer_is_running(record: &EmbedRecord) -> bool {
    crate::daemon_lock::pid_is_live(record.pid)
        && crate::boot_identity::is_recorded_process(
            record.pid,
            record.process_start,
            record.boot.as_deref(),
            crate::boot_identity::current().as_deref(),
        ) != Some(false)
}

/// A platform that cannot tell a live pid from a dead one takes every record for a running writer.
#[cfg(not(any(unix, windows)))]
fn writer_is_running(_record: &EmbedRecord) -> bool {
    true
}

/// Unix milliseconds now, or `0` if the clock is before the epoch.
pub(crate) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Documents the store tracks that still owe their vectors under `config`: embed is on for the
/// path, the entry's pass neither embedded nor attempted it.
#[cfg(feature = "documents")]
pub(crate) fn docs_owed(
    store: &crate::store::Store,
    config: &crate::config::Config,
    filters: &crate::scanner_filter::Filters,
) -> usize {
    if !config.documents.embed {
        return 0;
    }
    store
        .index
        .doc_files
        .iter()
        .filter(|(path, entry)| {
            !entry.embedded && !entry.embed_attempted && filters.doc_embed_allowed(path.to_str_lossy().as_ref())
        })
        .count()
}

/// The documents the store records as owing their vectors, in path order: neither embedded nor
/// attempted. Unlike [`docs_owed`] it reads no config: a document whose embedding has since been
/// turned off or excluded is listed too, and the scan handed it settles it by its stat.
#[cfg(feature = "documents")]
pub(crate) fn owed_doc_paths(store: &crate::store::Store) -> Vec<crate::path::RelPath> {
    let mut owed: Vec<_> = store
        .index
        .doc_files
        .iter()
        .filter(|(_, entry)| !entry.embedded && !entry.embed_attempted)
        .map(|(path, _)| path.clone())
        .collect();
    owed.sort_unstable();
    owed
}

#[cfg(test)]
#[path = "embed_progress_tests.rs"]
mod tests;
