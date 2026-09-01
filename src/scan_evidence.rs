//! Post-mortem evidence for a scan that died without getting to say why.
//!
//! Two instruments, one audience: whoever is holding the corpse of an OOM-killed process.
//!
//! **The breadcrumb.** [`ScanBreadcrumb::begin`] writes `<workspace_cache_dir>/scan-inflight.json`
//! *before* the work starts, and [`Drop`] removes it when the pass returns — by any path, including
//! an unwind. A file still sitting there afterwards therefore means exactly one thing: that process
//! never ran its destructors. That is precisely the signature `comms::store_health` cannot capture:
//! its record is written from `note_store_error`, inside a process still healthy enough to diagnose
//! itself, and a `SIGKILL` reaches none of that. The record's `phase` says how far the pass had got,
//! which is the question a memory post-mortem actually opens with.
//!
//! **The memory log.** [`log_memory_snapshot`] emits one `tracing` line with the process's current
//! footprint and the ceiling it is measured against. Sampled on an interval it reconstructs the
//! growth curve after the fact; issue #62 was filed against a process that left nothing behind at
//! all, so the curve had to be inferred from the size of the repository.
//!
//! This lives in core rather than under `comms` because a plain `basemind scan` needs both and must
//! not have to enable the `comms` feature to get them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::store_layout::{CACHE_DIR, WORKSPACES_DIR, cache_root, workspace_cache_dir};

/// Filename of the in-flight-scan breadcrumb inside a workspace cache dir.
const INFLIGHT_FILE: &str = "scan-inflight.json";

/// The record a live scan leaves behind, rewritten at each phase boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanInflight {
    /// Unix seconds at which the scan began. Not the phase timestamp: the age of the *pass* is what
    /// tells an operator whether the corpse is from this morning or from last month.
    pub started_unix: u64,
    /// The pid running the scan. The liveness test that separates a corpse from a scan in progress.
    pub pid: u32,
    /// The build that was scanning.
    pub version: String,
    /// The workspace root being scanned. Reported verbatim — a root of `/` is itself the finding.
    pub root: PathBuf,
    /// How far the pass had got. Drawn from a closed vocabulary; see [`ScanBreadcrumb::advance`].
    pub phase: String,
    /// Candidate files the pass was working through, once enumeration has produced a number.
    pub candidates: Option<usize>,
}

/// Path of the breadcrumb for a workspace cache dir.
pub fn inflight_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join(INFLIGHT_FILE)
}

/// Read the breadcrumb in a workspace cache dir, or `None` when there is none (or it is unreadable —
/// a truncated record is indistinguishable from no record for an operator report).
pub fn read(cache_dir: &Path) -> Option<ScanInflight> {
    let bytes = std::fs::read(inflight_path(cache_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Remove the breadcrumb in a workspace cache dir. Returns whether one was actually removed.
pub fn clear(cache_dir: &Path) -> bool {
    std::fs::remove_file(inflight_path(cache_dir)).is_ok()
}

/// A live scan's breadcrumb. Hold it for the duration of the pass; dropping it retracts the claim.
///
/// Deliberately not `Clone`: two owners would mean the first drop retracts a claim the second is
/// still making, which would report a dead scan as live and vice versa.
#[derive(Debug)]
pub struct ScanBreadcrumb {
    path: PathBuf,
    record: ScanInflight,
}

impl ScanBreadcrumb {
    /// Begin a breadcrumb for `root`, resolving its workspace cache dir. `None` when the record
    /// could not be written — the caller scans anyway; evidence is never a precondition for work.
    pub fn begin(root: &Path) -> Option<Self> {
        Self::begin_in(&workspace_cache_dir(root), root)
    }

    /// [`begin`](Self::begin) against an explicit cache dir, so tests need no global cache root.
    pub fn begin_in(cache_dir: &Path, root: &Path) -> Option<Self> {
        std::fs::create_dir_all(cache_dir).ok()?;
        let record = ScanInflight {
            started_unix: now_unix(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            root: root.to_path_buf(),
            phase: PHASE_STARTING.to_string(),
            candidates: None,
        };
        let breadcrumb = Self {
            path: inflight_path(cache_dir),
            record,
        };
        breadcrumb.write();
        Some(breadcrumb)
    }

    /// Record that the pass has reached `phase`, carrying the candidate count once enumeration has
    /// produced one.
    ///
    /// **Phase boundaries only.** This rewrites a file; called per file it would turn a diagnostic
    /// into a write amplifier on the hottest loop in the program, and the watcher's per-batch
    /// `scan_paths` would churn it continuously for no evidentiary gain. The phase constants in this
    /// module are the intended vocabulary.
    pub fn advance(&mut self, phase: &'static str, candidates: Option<usize>) {
        self.record.phase = phase.to_string();
        if candidates.is_some() {
            self.record.candidates = candidates;
        }
        self.write();
    }

    /// The record as it currently stands on disk.
    pub fn record(&self) -> &ScanInflight {
        &self.record
    }

    /// Where the breadcrumb is written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write via a sibling temp file plus a rename, so a reader never sees a half-written record.
    /// Best-effort throughout: a scan must not fail because its breadcrumb could not be updated.
    fn write(&self) {
        let Ok(bytes) = serde_json::to_vec_pretty(&self.record) else {
            return;
        };
        let temp = self.path.with_extension("json.tmp");
        if std::fs::write(&temp, bytes).is_ok() && std::fs::rename(&temp, &self.path).is_err() {
            let _ = std::fs::remove_file(&temp);
        }
    }
}

impl Drop for ScanBreadcrumb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The phase a breadcrumb starts in, before candidate enumeration has produced a count.
pub const PHASE_STARTING: &str = "starting";
/// Walking the tree / git revision to build the candidate list.
pub const PHASE_CANDIDATES: &str = "candidates";
/// The parallel per-file extraction pass.
pub const PHASE_EXTRACT: &str = "extract";
/// Applying outcomes and purging entries for files that are gone.
pub const PHASE_PURGE: &str = "purge";
/// Writing the code map — the durability barrier before the optional lanes.
pub const PHASE_FLUSH: &str = "flush";
/// The cross-file reference-resolution pass.
pub const PHASE_RESOLVE: &str = "resolve";
/// The optional post-extraction lanes (doc/code batches, removals, BM25 statistics).
pub const PHASE_LANES: &str = "lanes";

/// A breadcrumb whose process is gone: a scan that died without cleaning up after itself.
///
/// Gated to the platforms that can answer "is this pid live?" ([`crate::daemon_lock`] is itself
/// `unix`/`windows`-only). Writing and reading a breadcrumb needs no such answer and stays portable.
#[cfg(any(unix, windows))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaleScan {
    /// The workspace cache dir holding the record.
    pub cache_dir: PathBuf,
    /// The record itself.
    pub record: ScanInflight,
}

/// Every breadcrumb on this machine whose pid is no longer live.
///
/// Reads only the filesystem — no daemon RPC, no index open — so it stays usable on a machine that
/// is already wedged, which is the only machine anyone runs it on.
#[cfg(any(unix, windows))]
pub fn stale_inflight_scans() -> Vec<StaleScan> {
    stale_inflight_scans_in(&cache_root().join(CACHE_DIR).join(WORKSPACES_DIR))
}

/// [`stale_inflight_scans`] against an explicit workspaces dir, so tests need no global cache root.
///
/// A live pid is treated as a scan in progress and omitted. Pid reuse can therefore hide a genuine
/// corpse; the alternative — reporting every record — would flag every running scan as a crash,
/// which is the far commoner and far more misleading error.
#[cfg(any(unix, windows))]
pub fn stale_inflight_scans_in(workspaces_dir: &Path) -> Vec<StaleScan> {
    let Ok(entries) = std::fs::read_dir(workspaces_dir) else {
        return Vec::new();
    };
    let mut stale: Vec<StaleScan> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|cache_dir| {
            let record = read(&cache_dir)?;
            (!crate::daemon_lock::pid_is_live(record.pid)).then_some(StaleScan { cache_dir, record })
        })
        .collect();
    stale.sort_by_key(|scan| std::cmp::Reverse(scan.record.started_unix));
    stale
}

/// Unix seconds now, or `0` if the clock is before the epoch.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// How often [`memory_log_loop`] samples. Short enough to catch the shape of a runaway (a scan can
/// go from healthy to killed inside a minute), long enough that the log stays readable over hours.
pub const MEMORY_LOG_EVERY: Duration = Duration::from_secs(5);

/// Emit one memory line for the current process: what it is using, the ceiling it is using it
/// against, how long it has been going, and which root it is attributed to.
///
/// Nothing else records this. When the kernel kills the process there is no exit path to log from,
/// so the only account of the growth that led there is the one already written.
pub fn log_memory_snapshot(root: &Path, elapsed: Duration) {
    let Some(reading) = crate::sysres::memory_reading() else {
        return;
    };
    tracing::info!(
        rss_mb = reading.used_bytes / (1 << 20),
        ceiling_mb = reading.limit_bytes.map(|limit| limit / (1 << 20)),
        ceiling_enforced = reading.limit_is_enforced,
        source = reading.source.as_str(),
        elapsed_secs = elapsed.as_secs(),
        root = %root.display(),
        "memory"
    );
}

/// Samples [`log_memory_snapshot`] every [`MEMORY_LOG_EVERY`] on its own thread until dropped.
///
/// A plain thread rather than a tokio task because the thing worth watching is
/// [`crate::scanner::scan`], which is synchronous by design — the scanner pipeline runs on rayon
/// with no reactor on the path (see `.ai-rulez/context/scanner-pipeline.md`). An async sampler
/// could only be owned by an async caller, which the CLI scan is not, and the CLI is exactly where
/// someone reproduces an out-of-memory report.
///
/// Cancellation is a `Condvar` wait rather than a sleep so the thread joins promptly at the end of
/// a scan instead of holding the process open for up to [`MEMORY_LOG_EVERY`].
pub struct MemoryLog {
    stop: Arc<(Mutex<bool>, Condvar)>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MemoryLog {
    /// Start sampling for `root`. Returns `None` if the thread cannot be spawned — evidence
    /// gathering must never be the reason a scan fails to run.
    pub fn start(root: &Path) -> Option<Self> {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_stop = Arc::clone(&stop);
        let root = root.to_path_buf();
        let handle = std::thread::Builder::new()
            .name("basemind-memlog".to_string())
            .spawn(move || {
                let started = std::time::Instant::now();
                let (lock, cvar) = &*worker_stop;
                let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                loop {
                    // Test the predicate BEFORE waiting, not only after. A `Drop` that runs before
                    // this thread first reaches `wait_timeout` notifies an empty wait set, and a
                    // condvar keeps no record of that -- so a wait entered afterwards would sleep
                    // out the full interval on a stop that has already been requested. ~keep
                    if *guard {
                        return;
                    }
                    let (next, _) = cvar
                        .wait_timeout(guard, MEMORY_LOG_EVERY)
                        .unwrap_or_else(PoisonError::into_inner);
                    guard = next;
                    if *guard {
                        return;
                    }
                    drop(guard);
                    log_memory_snapshot(&root, started.elapsed());
                    guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                }
            })
            .ok()?;
        Some(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for MemoryLog {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.stop;
        *lock.lock().unwrap_or_else(PoisonError::into_inner) = true;
        cvar.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dropping the sampler must stop it *promptly*. A `thread::sleep` loop would hold the process
    /// open for up to `MEMORY_LOG_EVERY` past the end of every scan, which on the CLI is a five
    /// second pause after the summary has already printed — the kind of regression that gets
    /// evidence-gathering ripped out rather than fixed. The margin is deliberately loose; what is
    /// being pinned is "cancels on notify", not a latency budget.
    #[test]
    fn the_memory_log_stops_when_dropped_instead_of_sleeping_out_its_interval() {
        let started = std::time::Instant::now();
        let log = MemoryLog::start(Path::new("/repo/under/test")).expect("sampler thread spawns");
        drop(log);
        let elapsed = started.elapsed();
        assert!(
            elapsed < MEMORY_LOG_EVERY,
            "drop must join on the condvar, not wait out {MEMORY_LOG_EVERY:?} (took {elapsed:?})"
        );
    }

    #[test]
    fn a_breadcrumb_is_written_on_begin_and_removed_on_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().join("workspaces").join("abc123");
        let root = PathBuf::from("/repo/under/test");

        assert_eq!(read(&cache), None, "nothing before the scan starts");
        {
            let breadcrumb = ScanBreadcrumb::begin_in(&cache, &root).expect("breadcrumb written");
            let record = read(&cache).expect("the record is on disk while the scan runs");
            assert_eq!(record.pid, std::process::id());
            assert_eq!(record.root, root);
            assert_eq!(record.phase, PHASE_STARTING);
            assert_eq!(record.candidates, None);
            assert_eq!(breadcrumb.path(), inflight_path(&cache));
        }
        assert_eq!(read(&cache), None, "a pass that returns retracts its claim");
    }

    #[test]
    fn advance_rewrites_the_phase_and_retains_the_candidate_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = PathBuf::from("/repo");
        let mut breadcrumb = ScanBreadcrumb::begin_in(dir.path(), &root).expect("breadcrumb written");

        breadcrumb.advance(PHASE_EXTRACT, Some(4096));
        let extracting = read(dir.path()).expect("record present");
        assert_eq!(extracting.phase, PHASE_EXTRACT);
        assert_eq!(extracting.candidates, Some(4096));

        // A later phase knows no new count; the enumerated one must survive rather than be erased.
        breadcrumb.advance(PHASE_RESOLVE, None);
        let resolving = read(dir.path()).expect("record present");
        assert_eq!(resolving.phase, PHASE_RESOLVE);
        assert_eq!(resolving.candidates, Some(4096));
        assert_eq!(resolving.started_unix, extracting.started_unix, "the pass began once");
    }

    /// The whole point of the breadcrumb. `mem::forget` is a faithful stand-in for `SIGKILL`: no
    /// destructor runs, so the file outlives the pass exactly as it does after an OOM kill.
    #[test]
    fn a_breadcrumb_survives_a_process_that_never_runs_its_destructors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let breadcrumb = ScanBreadcrumb::begin_in(dir.path(), Path::new("/repo")).expect("breadcrumb written");
        std::mem::forget(breadcrumb);

        let record = read(dir.path()).expect("a hard death leaves the record behind");
        assert_eq!(record.pid, std::process::id());
        assert!(clear(dir.path()), "and it can be acknowledged");
        assert_eq!(read(dir.path()), None);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn only_records_whose_process_is_gone_count_as_stale() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspaces = dir.path().join("workspaces");

        let live = workspaces.join("live");
        let live_crumb = ScanBreadcrumb::begin_in(&live, Path::new("/repo/live")).expect("live breadcrumb");

        // Past every platform's pid ceiling, so it can never be a live process. Not 0: `kill(0, 0)`
        // signals the *caller's* process group and succeeds, which reads back as live.
        let dead = workspaces.join("dead");
        std::fs::create_dir_all(&dead).expect("create dead cache dir");
        let corpse = ScanInflight {
            started_unix: 1,
            pid: 0x7FFF_FFFE,
            version: "0.0.0".to_string(),
            root: PathBuf::from("/"),
            phase: PHASE_EXTRACT.to_string(),
            candidates: Some(9),
        };
        std::fs::write(
            inflight_path(&dead),
            serde_json::to_vec(&corpse).expect("encode corpse"),
        )
        .expect("write corpse");

        let stale = stale_inflight_scans_in(&workspaces);
        assert_eq!(stale.len(), 1, "the running scan is not a crash: {stale:?}");
        assert_eq!(stale[0].cache_dir, dead);
        assert_eq!(stale[0].record.root, PathBuf::from("/"));
        assert_eq!(stale[0].record.phase, PHASE_EXTRACT);
        drop(live_crumb);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn enumerating_a_missing_workspaces_dir_reports_nothing_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(stale_inflight_scans_in(&dir.path().join("absent")).is_empty());
    }

    #[test]
    fn a_snapshot_can_be_logged_without_a_subscriber_installed() {
        log_memory_snapshot(Path::new("/repo"), Duration::from_secs(7));
    }
}
