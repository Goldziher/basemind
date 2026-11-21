//! Post-mortem evidence for a scan that died without getting to say why.
//!
//! Two instruments, one audience: whoever is holding the corpse of an OOM-killed process.
//!
//! **The breadcrumb.** [`ScanBreadcrumb::begin`] writes `<workspace_cache_dir>/scan-inflight.json`
//! *before* the work starts, and [`Drop`] removes it when the pass returns — by any path, including
//! an unwind. A file still sitting there afterwards therefore means exactly one thing: that process
//! never ran its destructors, which no record written by the process itself can capture: a
//! `SIGKILL` reaches none of that code. The record's `phase` says how far the pass had got,
//! which is the question a memory post-mortem actually opens with. While its process lives, the same
//! record is the scan's progress: [`live_progress`] turns its counts into what `index_status` and the
//! statusline report.
//!
//! **The memory log.** [`log_memory_snapshot`] emits one `tracing` line with the process's current
//! footprint and the ceiling it is measured against. Sampled on an interval it reconstructs the
//! growth curve after the fact; issue #62 was filed against a process that left nothing behind at
//! all, so the curve had to be inferred from the size of the repository.
//!
//! This lives in core rather than under `comms` because a plain `basemind scan` needs both and must
//! not have to enable the `comms` feature to get them.

use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::data_root::DataRoot;
use crate::store_layout::workspace_cache_dir;

/// Filename of the in-flight-scan breadcrumb inside a workspace cache dir.
const INFLIGHT_FILE: &str = "scan-inflight.json";

/// The record a live scan leaves behind, rewritten at each phase boundary and each drive chunk.
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
    /// Unix milliseconds at which `phase` began. Absent in a record from an older build.
    #[serde(default)]
    pub phase_started_unix_ms: Option<u64>,
    /// Candidates the extract phase has finished, rewritten once per drive chunk. Absent until
    /// extraction finishes its first chunk.
    #[serde(default)]
    pub files_done: Option<usize>,
    /// The scanning process's [`crate::boot_identity::process_start`], which tells it from a later
    /// process given its pid. Absent in a record from an older build, or where the platform gives none.
    #[serde(default)]
    pub process_start: Option<u64>,
    /// The boot the scan ran in ([`crate::boot_identity::current`]), without which a Linux start
    /// from an earlier boot could repeat. Absent as `process_start` is.
    #[serde(default)]
    pub boot: Option<String>,
}

/// A running scan's progress, as counts from its breadcrumb.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct ScanProgress {
    /// The phase the scan is in: `starting`, `candidates`, `extract`, `purge`, `flush`, `resolve`
    /// or `lanes`.
    pub phase: String,
    /// Candidate files extracted so far.
    pub files_done: usize,
    /// Candidate files the scan works through. Absent while it still lists them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_total: Option<usize>,
    /// Seconds until extraction finishes, at the rate it has run so far. Absent outside the extract
    /// phase and before its first chunk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<u64>,
}

impl ScanInflight {
    /// The progress this record describes at `now_unix_ms`.
    pub fn progress(&self, now_unix_ms: u64) -> ScanProgress {
        let files_done = self.files_done.unwrap_or(0);
        let eta_secs = match (self.phase.as_str(), self.phase_started_unix_ms, self.candidates) {
            (PHASE_EXTRACT, Some(started), Some(total)) => {
                eta_from_rate(now_unix_ms.saturating_sub(started), files_done, total)
            }
            _ => None,
        };
        ScanProgress {
            phase: self.phase.clone(),
            files_done,
            files_total: self.candidates,
            eta_secs,
        }
    }
}

/// Seconds left for the `total - done` files still to go, at the rate `done` files took
/// `elapsed_ms`, rounded up. `None` when no rate is known yet or nothing is left.
fn eta_from_rate(elapsed_ms: u64, done: usize, total: usize) -> Option<u64> {
    if done == 0 || done >= total || elapsed_ms == 0 {
        return None;
    }
    let remaining = u128::from(elapsed_ms) * (total - done) as u128;
    u64::try_from(remaining.div_ceil(done as u128 * 1000)).ok()
}

/// The progress of the scan running now for the workspace in `cache_dir`. `None` when no scan is
/// running there: no breadcrumb, or one whose process is gone or whose pid another process now holds.
pub fn live_progress(cache_dir: &Path) -> Option<ScanProgress> {
    read(cache_dir)
        .filter(scan_process_is_running)
        .map(|record| record.progress(now_unix_ms()))
}

#[cfg(any(unix, windows))]
fn scan_process_is_running(record: &ScanInflight) -> bool {
    crate::daemon_lock::pid_is_live(record.pid)
        && crate::boot_identity::is_recorded_process(
            record.pid,
            record.process_start,
            record.boot.as_deref(),
            crate::boot_identity::current().as_deref(),
        ) != Some(false)
}

/// A platform that cannot tell a live pid from a dead one takes every breadcrumb for a running scan.
#[cfg(not(any(unix, windows)))]
fn scan_process_is_running(_record: &ScanInflight) -> bool {
    true
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
    note_unclean_scan(cache_dir);
    std::fs::remove_file(inflight_path(cache_dir)).is_ok()
}

/// Marker in a workspace cache dir: a scan of it never finished, so blobs the file map names may
/// be torn. Blobs are written without an fsync while the file map is fsynced, so after a power
/// loss the map can name a blob whose bytes never reached the disk.
pub(crate) const VERIFY_BLOBS_FILE: &str = "verify-blobs";

/// Turn a breadcrumb found in `cache_dir` into the durable request to verify blobs. Called wherever
/// a breadcrumb is about to be replaced or removed by anything but the scan that wrote it, since a
/// breadcrumb nobody retracted is the only trace an unclean shutdown leaves.
fn note_unclean_scan(cache_dir: &Path) {
    if inflight_path(cache_dir).exists() {
        let _ = std::fs::write(cache_dir.join(VERIFY_BLOBS_FILE), b"");
    }
}

/// Whether the next scan of this workspace must be a full one that validates each blob it would
/// otherwise trust for merely existing.
pub fn blobs_need_verifying(cache_dir: &Path) -> bool {
    cache_dir.join(VERIFY_BLOBS_FILE).exists()
}

/// A full scan validated every blob it kept: withdraw the request.
pub fn blobs_verified(cache_dir: &Path) {
    let _ = std::fs::remove_file(cache_dir.join(VERIFY_BLOBS_FILE));
}

pub use crate::scan_evidence_hosts::{HostingMarker, settle_dead_hosts};

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
        note_unclean_scan(cache_dir);
        let record = ScanInflight {
            started_unix: now_unix(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            root: root.to_path_buf(),
            phase: PHASE_STARTING.to_string(),
            candidates: None,
            phase_started_unix_ms: Some(now_unix_ms()),
            files_done: None,
            process_start: crate::boot_identity::process_start(std::process::id()),
            boot: crate::boot_identity::current(),
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
        self.advance_at(phase, candidates, now_unix_ms());
    }

    /// [`advance`](Self::advance) with the phase's start time given, so tests need no clock.
    pub fn advance_at(&mut self, phase: &'static str, candidates: Option<usize>, now_unix_ms: u64) {
        self.record.phase = phase.to_string();
        self.record.phase_started_unix_ms = Some(now_unix_ms);
        if candidates.is_some() {
            self.record.candidates = candidates;
        }
        self.write();
    }

    /// Record that the extract phase has finished `done` candidates. Called once per drive chunk
    /// (about a thousand files), never per file, for the reason [`advance`](Self::advance) gives.
    pub fn files_done(&mut self, done: usize) {
        self.record.files_done = Some(done);
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
        // The breadcrumb is the evidence that blobs written without an fsync may be torn: it goes
        // only once they are on disk, or a power loss seconds after a finished scan tore blobs no
        // record pointed at. A process holding a hosting marker leaves the sync to that marker's
        // removal: its host holds the store's write lock here, and the sync can take seconds. ~keep
        if !crate::scan_evidence_hosts::is_hosting() {
            sync_blob_filesystem();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Write back every dirty page of the filesystem holding the blob store, once per full pass. Linux
/// only: there the delayed allocation of ext4 and XFS leaves a renamed blob empty after a power
/// loss. Best-effort, logged on failure.
pub(crate) fn sync_blob_filesystem() {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd as _;

        let blobs = crate::store_layout::global_blobs_dir();
        let Ok(dir) = std::fs::File::open(&blobs) else {
            return;
        };
        // SAFETY: `syncfs` takes an open file descriptor and touches no memory of ours; `dir` keeps
        // the descriptor open for the duration of the call.
        if unsafe { libc::syncfs(dir.as_raw_fd()) } != 0 {
            let error = std::io::Error::last_os_error();
            tracing::warn!(%error, path = %blobs.display(), "could not write the blob store back to disk");
        }
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
pub fn stale_inflight_scans(data: &DataRoot) -> Vec<StaleScan> {
    stale_inflight_scans_in(&data.workspaces_dir())
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

/// Unix milliseconds now, or `0` if the clock is before the epoch.
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// How often a memory line is written. Short enough to catch the shape of a runaway (a scan can
/// go from healthy to killed inside a minute), long enough that the log stays readable over hours.
pub const MEMORY_LOG_EVERY: Duration = Duration::from_secs(5);

/// How often the sampler thread reads the footprint between lines. A line written every
/// [`MEMORY_LOG_EVERY`] sees one instant in fifty; a transient (one pathological file's arenas,
/// grown and freed inside a couple of seconds) peaked above 12 GB while the lines showed 5 GB. The
/// reading is one cheap syscall, so sampling at this rate costs nothing measurable, and the
/// window's maximum is reported on the next line (`window_peak_mb`).
pub const MEMORY_SAMPLE_EVERY: Duration = Duration::from_millis(200);

/// A reading this far above the highest footprint already written triggers an immediate extra
/// line, so a spike is on the record even if the process is killed before the next scheduled one.
/// Each such line raises the bar, so a climbing process writes a handful, not a flood.
pub const MEMORY_SPIKE_STEP_BYTES: u64 = 1 << 30;

/// Emit one memory line for the current process: what it is using, the ceiling it is using it
/// against, how long it has been going, and which root it is attributed to.
///
/// Nothing else records this. When the kernel kills the process there is no exit path to log from,
/// so the only account of the growth that led there is the one already written.
pub fn log_memory_snapshot(root: &Path, elapsed: Duration) {
    let Some(reading) = crate::sysres::memory_reading() else {
        return;
    };
    log_memory_reading(root, elapsed, &reading, reading.used_bytes, false);
}

/// Write one line for `reading`. `window_peak_bytes` is the highest footprint sampled since the
/// previous line (never below `reading.used_bytes`); `spike` marks a line written between the
/// scheduled ones because the footprint jumped.
fn log_memory_reading(
    root: &Path,
    elapsed: Duration,
    reading: &crate::sysres::MemoryReading,
    window_peak_bytes: u64,
    spike: bool,
) {
    tracing::info!(
        rss_mb = reading.used_bytes / (1 << 20),
        window_peak_mb = window_peak_bytes.max(reading.used_bytes) / (1 << 20),
        process_peak_rss_mb = crate::sysres::sample().peak_bytes.map(|peak| peak / (1 << 20)),
        spike,
        ceiling_mb = reading.limit_bytes.map(|limit| limit / (1 << 20)),
        ceiling_enforced = reading.limit_is_enforced,
        source = reading.source.as_str(),
        elapsed_secs = elapsed.as_secs(),
        root = %root.display(),
        "memory"
    );
}

/// Tracks the sampler's window maximum and decides when a reading deserves a line of its own.
#[derive(Debug, Default)]
struct PeakWindow {
    /// Highest footprint sampled since the last written line.
    window_peak: u64,
    /// Highest footprint any written line has carried so far.
    written_high: u64,
}

impl PeakWindow {
    /// Fold in one sample. Returns true when it is [`MEMORY_SPIKE_STEP_BYTES`] above everything
    /// written so far and so should be written now.
    fn observe(&mut self, used_bytes: u64) -> bool {
        self.window_peak = self.window_peak.max(used_bytes);
        used_bytes >= self.written_high.saturating_add(MEMORY_SPIKE_STEP_BYTES)
    }

    /// A line was written: report its window peak, start a new window, raise the spike bar.
    fn written(&mut self) -> u64 {
        let peak = self.window_peak;
        self.written_high = self.written_high.max(peak);
        self.window_peak = 0;
        peak
    }
}

/// Samples the footprint every [`MEMORY_SAMPLE_EVERY`] and writes a line every [`MEMORY_LOG_EVERY`]
/// (or at once on a jump of [`MEMORY_SPIKE_STEP_BYTES`]) for one scan, until dropped.
///
/// The sampling runs on a plain thread rather than a tokio task because the thing worth watching
/// is [`crate::scanner::scan`], which is synchronous by design — the scanner pipeline runs on rayon
/// with no reactor on the path (see `.ai-rulez/context/scanner-pipeline.md`). An async sampler
/// could only be owned by an async caller, which the CLI scan is not, and the CLI is exactly where
/// someone reproduces an out-of-memory report.
///
/// One process-wide thread serves every live log. A thread per scan was a thread created and
/// destroyed for every rescan a long-lived daemon ran, a few per edit an agent made. The footprint
/// is the process's, so one reading per tick serves every scan in flight. The thread waits without
/// a timeout while no scan runs, so an idle process pays no wakeups for it.
pub struct MemoryLog {
    id: u64,
}

/// One scan's sampling state, kept by the shared sampler.
struct MemorySession {
    id: u64,
    root: PathBuf,
    peaks: crate::scan_phase_peaks::PhasePeaks,
    started: std::time::Instant,
    window: PeakWindow,
    last_line: std::time::Instant,
}

impl MemorySession {
    fn observe(&mut self, reading: &crate::sysres::MemoryReading) {
        self.peaks.observe(reading.used_bytes);
        let spike = self.window.observe(reading.used_bytes);
        if spike || self.last_line.elapsed() >= MEMORY_LOG_EVERY {
            let peak = self.window.written();
            log_memory_reading(&self.root, self.started.elapsed(), reading, peak, spike);
            self.last_line = std::time::Instant::now();
        }
    }
}

/// The live logs and the condvar their sampler waits on.
struct MemorySampler {
    sessions: Mutex<Vec<MemorySession>>,
    wake: Condvar,
}

static SAMPLER: MemorySampler = MemorySampler {
    sessions: Mutex::new(Vec::new()),
    wake: Condvar::new(),
};

/// Sampler threads this process has started: one at most. A regression guard reads it.
static SAMPLER_SPAWNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl MemorySampler {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<MemorySession>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the sampler thread runs, starting it on first use.
    fn running() -> bool {
        static RUNNING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *RUNNING.get_or_init(|| {
            SAMPLER_SPAWNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::thread::Builder::new()
                .name("basemind-memlog".to_string())
                .spawn(|| SAMPLER.run())
                .is_ok()
        })
    }

    /// Sample every [`MEMORY_SAMPLE_EVERY`] while any log is live. Each log's lines are written
    /// under the lock its [`Drop`] takes, so no line for a scan is written after its log is dropped.
    fn run(&self) {
        let mut sessions = self.lock();
        loop {
            if sessions.is_empty() {
                sessions = self.wake.wait(sessions).unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            sessions = self
                .wake
                .wait_timeout(sessions, MEMORY_SAMPLE_EVERY)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            if sessions.is_empty() {
                continue;
            }
            drop(sessions);
            let reading = crate::sysres::memory_reading();
            sessions = self.lock();
            if let Some(reading) = reading {
                for session in sessions.iter_mut() {
                    session.observe(&reading);
                }
            }
        }
    }
}

impl MemoryLog {
    /// Start sampling for `root`. Returns `None` if the sampler thread cannot be spawned —
    /// evidence gathering must never be the reason a scan fails to run.
    pub fn start(root: &Path, peaks: crate::scan_phase_peaks::PhasePeaks) -> Option<Self> {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if !MemorySampler::running() {
            return None;
        }
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let now = std::time::Instant::now();
        let mut sessions = SAMPLER.lock();
        sessions.push(MemorySession {
            id,
            root: root.to_path_buf(),
            peaks,
            started: now,
            window: PeakWindow::default(),
            last_line: now,
        });
        if sessions.len() == 1 {
            SAMPLER.wake.notify_all();
        }
        Some(Self { id })
    }
}

impl Drop for MemoryLog {
    fn drop(&mut self) {
        SAMPLER.lock().retain(|session| session.id != self.id);
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
        let log = MemoryLog::start(Path::new("/repo/under/test"), Default::default()).expect("sampler thread spawns");
        drop(log);
        let elapsed = started.elapsed();
        assert!(
            elapsed < MEMORY_LOG_EVERY,
            "drop must join on the condvar, not wait out {MEMORY_LOG_EVERY:?} (took {elapsed:?})"
        );
    }

    /// Every scan shares one sampler thread: a daemon rescanning on every edit must not create and
    /// destroy a thread per scan.
    #[test]
    fn every_memory_log_shares_one_sampler_thread() {
        for _ in 0..16 {
            let log = MemoryLog::start(Path::new("/repo/under/test"), Default::default()).expect("sampler runs");
            drop(log);
        }
        let overlapping: Vec<MemoryLog> = (0..4)
            .filter_map(|_| MemoryLog::start(Path::new("/repo/under/test"), Default::default()))
            .collect();
        assert_eq!(overlapping.len(), 4);
        assert_eq!(SAMPLER_SPAWNS.load(std::sync::atomic::Ordering::Relaxed), 1);
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

    #[test]
    fn the_record_reports_files_done_of_the_total_and_an_eta_at_the_rate_so_far() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut breadcrumb = ScanBreadcrumb::begin_in(dir.path(), Path::new("/repo")).expect("breadcrumb written");
        breadcrumb.advance_at(PHASE_EXTRACT, Some(4000), 10_000);
        breadcrumb.files_done(1000);

        let record = read(dir.path()).expect("record present");
        assert_eq!(record.phase_started_unix_ms, Some(10_000));
        assert_eq!(record.files_done, Some(1000));
        // 1000 files took 30 s, so the 3000 left take 90 s.
        assert_eq!(
            record.progress(40_000),
            ScanProgress {
                phase: PHASE_EXTRACT.to_string(),
                files_done: 1000,
                files_total: Some(4000),
                eta_secs: Some(90),
            }
        );
    }

    #[test]
    fn progress_outside_a_running_extraction_carries_no_eta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut breadcrumb = ScanBreadcrumb::begin_in(dir.path(), Path::new("/repo")).expect("breadcrumb written");
        breadcrumb.advance_at(PHASE_CANDIDATES, None, 1_000);
        let listing = ScanProgress {
            phase: PHASE_CANDIDATES.to_string(),
            files_done: 0,
            files_total: None,
            eta_secs: None,
        };
        assert_eq!(read(dir.path()).expect("record").progress(5_000), listing);

        breadcrumb.advance_at(PHASE_EXTRACT, Some(8), 2_000);
        let before_first_chunk = read(dir.path()).expect("record").progress(5_000);
        assert_eq!((before_first_chunk.files_done, before_first_chunk.eta_secs), (0, None));

        breadcrumb.files_done(8);
        breadcrumb.advance_at(PHASE_RESOLVE, None, 3_000);
        let resolving = ScanProgress {
            phase: PHASE_RESOLVE.to_string(),
            files_done: 8,
            files_total: Some(8),
            eta_secs: None,
        };
        assert_eq!(read(dir.path()).expect("record").progress(9_000), resolving);
    }

    #[test]
    fn the_eta_is_the_remaining_files_at_the_observed_rate_rounded_up() {
        let cases = [
            (30_000, 1000, 4000, Some(90)),
            (1, 3, 4, Some(1)),
            (2_500, 1, 2, Some(3)),
            (0, 10, 20, None),
            (5_000, 0, 10, None),
            (5_000, 10, 10, None),
            (5_000, 12, 10, None),
        ];
        for (elapsed_ms, done, total, expected) in cases {
            assert_eq!(
                eta_from_rate(elapsed_ms, done, total),
                expected,
                "{elapsed_ms} ms, {done}/{total}"
            );
        }
    }

    #[test]
    fn a_record_from_an_older_build_reads_with_no_progress_counts() {
        let old = r#"{"started_unix":1,"pid":1,"version":"0.34.0","root":"/repo","phase":"extract","candidates":9}"#;
        let record: ScanInflight = serde_json::from_str(old).expect("an older record still parses");
        assert_eq!((record.phase_started_unix_ms, record.files_done), (None, None));
        assert_eq!(record.progress(1_000).eta_secs, None);
    }

    #[test]
    fn live_progress_is_reported_only_while_the_scanning_process_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(live_progress(dir.path()), None, "no breadcrumb, no scan");

        let mut breadcrumb = ScanBreadcrumb::begin_in(dir.path(), Path::new("/repo")).expect("breadcrumb written");
        breadcrumb.advance(PHASE_EXTRACT, Some(3));
        breadcrumb.files_done(2);
        let live = live_progress(dir.path()).expect("this process is scanning");
        assert_eq!((live.files_done, live.files_total), (2, Some(3)));

        drop(breadcrumb);
        assert_eq!(live_progress(dir.path()), None, "a finished scan reports no progress");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn the_breadcrumb_of_a_dead_scan_is_not_progress() {
        let dir = tempfile::tempdir().expect("tempdir");
        let corpse = ScanInflight {
            started_unix: 1,
            pid: 0x7FFF_FFFE,
            version: "0.0.0".to_string(),
            root: PathBuf::from("/repo"),
            phase: PHASE_EXTRACT.to_string(),
            candidates: Some(9),
            phase_started_unix_ms: Some(1_000),
            files_done: Some(4),
            process_start: None,
            boot: None,
        };
        std::fs::write(inflight_path(dir.path()), serde_json::to_vec(&corpse).expect("encode")).expect("write");
        assert_eq!(live_progress(dir.path()), None);
    }

    /// A scan's pid reused by another process is not the scan: a live pid whose recorded start is
    /// not the one it now has reports no progress, while the record of this very process does.
    #[cfg(any(unix, windows))]
    #[test]
    fn the_breadcrumb_of_a_scan_whose_pid_another_process_holds_is_not_progress() {
        let start = crate::boot_identity::process_start(std::process::id()).expect("this process has a start");
        let record = |process_start| ScanInflight {
            started_unix: 1,
            pid: std::process::id(),
            version: "0.0.0".to_string(),
            root: PathBuf::from("/repo"),
            phase: PHASE_EXTRACT.to_string(),
            candidates: Some(9),
            phase_started_unix_ms: Some(1_000),
            files_done: Some(4),
            process_start,
            boot: crate::boot_identity::current(),
        };
        let dir = tempfile::tempdir().expect("tempdir");

        let write = |record: &ScanInflight| {
            std::fs::write(inflight_path(dir.path()), serde_json::to_vec(record).expect("encode")).expect("write");
        };
        write(&record(Some(start.wrapping_add(1))));
        assert_eq!(
            live_progress(dir.path()),
            None,
            "another process started at another time"
        );
        write(&record(Some(start)));
        assert!(live_progress(dir.path()).is_some(), "the recorded process itself");
        write(&record(None));
        assert!(live_progress(dir.path()).is_some(), "an older record names no start");
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
            phase_started_unix_ms: None,
            files_done: None,
            process_start: None,
            boot: None,
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

    /// The 12 GB transient of issue #77 lived for under a scheduled interval: the window maximum
    /// must survive until the next line, and only a jump past everything written triggers one early.
    #[test]
    fn the_peak_window_keeps_the_maximum_and_flags_only_new_highs() {
        const GIB: u64 = 1 << 30;
        let mut window = PeakWindow::default();
        assert!(!window.observe(GIB / 2), "a small first reading is not a spike");
        assert!(
            window.observe(2 * GIB),
            "a jump of a step over nothing written is a spike"
        );
        assert_eq!(window.written(), 2 * GIB, "the line carries the window maximum");

        assert!(!window.observe(GIB), "falling back is not a spike");
        assert!(
            !window.observe(2 * GIB + GIB / 2),
            "less than a step above the written high"
        );
        assert!(
            window.observe(3 * GIB),
            "a further step over the written high is a spike"
        );
        assert_eq!(window.written(), 3 * GIB);
        assert_eq!(window.written(), 0, "a fresh window starts empty");
    }

    #[test]
    fn sampling_is_finer_than_logging() {
        assert!(MEMORY_SAMPLE_EVERY * 10 <= MEMORY_LOG_EVERY);
    }

    #[test]
    fn a_snapshot_can_be_logged_without_a_subscriber_installed() {
        log_memory_snapshot(Path::new("/repo"), Duration::from_secs(7));
    }
}
