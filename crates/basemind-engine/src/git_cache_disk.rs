//! Disk tier of the git cache: the payload directories under `git-cache/` and the byte budget that
//! bounds each of them.
//!
//! The budget is enforced off the request path, on two occasions. Opening the tier starts a sweep on
//! a background thread, which measures what earlier processes left behind. After that every write
//! adds its bytes to a running total for its directory, and the write that takes a directory past
//! its budget starts another background sweep, so a daemon that hosts a workspace for days stays
//! within budget without statting the directories on a request. One sweep runs at a time: a write
//! past the budget while one runs leaves a request for it, and the running sweep passes over every
//! directory again before it ends, so a directory it had already visited is not left over budget.
//!
//! A sweep is an mtime-LRU pass: stat every file, sum the sizes, and when the sum is over budget
//! delete the oldest payloads until it is at most `low_water` of the budget. Stopping below the
//! budget leaves room for the writes that follow, so a directory at its budget does not sweep again
//! on the next write. The sweep then resets the running total to the bytes it kept. None of the
//! directories holds immortal entries: `log/` keys on HEAD, `blame/` on the HEAD a blame was asked
//! at, so every HEAD move mints new entries in both, and a payload of an older schema is never read
//! again in any of them. Errors during traversal and removal are swallowed: cache size is
//! best-effort, not load-bearing.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Default disk budget for each subdirectory of the cache (256 MiB). Tuneable via
/// `BASEMIND_GIT_CACHE_LOG_MAX_BYTES`, which despite its name sets every subdirectory's budget;
/// setting it to 0 disables eviction.
pub const LOG_CACHE_DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// A `*.tmp` file older than this was left by a writer that died between its write and its rename.
const STALE_TMP_AGE: Duration = Duration::from_secs(60 * 60);

/// A sweep over budget evicts until a tenth of the budget is free again.
const SWEEP_HEADROOM_DIVISOR: u64 = 10;

/// One flat directory of payload files under the cache root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subdir {
    Log,
    Blame,
    CommitFiles,
}

impl Subdir {
    /// Every subdirectory, in the order a sweep visits them.
    pub(crate) const ALL: [Self; 3] = [Self::Log, Self::Blame, Self::CommitFiles];

    /// The directory's name under the cache root.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Blame => "blame",
            Self::CommitFiles => "commit_files",
        }
    }

    fn slot(self) -> usize {
        match self {
            Self::Log => 0,
            Self::Blame => 1,
            Self::CommitFiles => 2,
        }
    }
}

/// The cache's directories, their budget and the bytes written to each since it was last measured.
pub struct DiskTier {
    root: PathBuf,
    /// Byte budget of each subdirectory; `0` disables eviction.
    max_bytes: u64,
    /// Running byte total per [`Subdir::slot`]: the bytes the last sweep kept plus every write since.
    /// It overcounts a payload overwritten in place and one a concurrent sweep already measured,
    /// which only brings the next sweep forward; that sweep measures again.
    tracked: [AtomicU64; 3],
    /// Set while a background sweep runs, so a burst of writes past the budget starts one sweep.
    sweeping: AtomicBool,
    /// Set by every request for a sweep and cleared by the pass that serves it, so a request made
    /// while a sweep runs gets a pass of its own.
    pending: AtomicBool,
}

impl DiskTier {
    /// A tier over the existing subdirectories of `root`, each held to `max_bytes`. Nothing is
    /// measured until the first sweep.
    pub(crate) fn new(root: PathBuf, max_bytes: u64) -> Arc<Self> {
        Arc::new(Self {
            root,
            max_bytes,
            tracked: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
            sweeping: AtomicBool::new(false),
            pending: AtomicBool::new(false),
        })
    }

    /// The cache root.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// The directory of `sub`.
    pub(crate) fn dir(&self, sub: Subdir) -> PathBuf {
        self.root.join(sub.name())
    }

    /// Write `bytes` to `path` in `sub` and charge them to its running total. The write that takes
    /// the total past the budget starts a background sweep. A failed write is dropped: the value is
    /// recomputed on the next miss.
    pub(crate) fn write(self: &Arc<Self>, sub: Subdir, path: &Path, bytes: &[u8]) {
        if atomic_write(path, bytes).is_err() {
            return;
        }
        let len = bytes.len() as u64;
        let total = self.tracked[sub.slot()]
            .fetch_add(len, Ordering::AcqRel)
            .saturating_add(len);
        if self.max_bytes != 0 && total > self.max_bytes {
            self.sweep_in_background();
        }
    }

    /// Request a sweep and start one on its own thread unless one is already running, in which case
    /// the running one serves the request before it ends.
    pub(crate) fn sweep_in_background(self: &Arc<Self>) {
        self.pending.store(true, Ordering::SeqCst);
        if self.sweeping.swap(true, Ordering::SeqCst) {
            return;
        }
        let tier = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("bm-git-cache-gc".to_string())
            .spawn(move || tier.run_sweeps());
        if let Err(error) = spawned {
            self.sweeping.store(false, Ordering::SeqCst);
            tracing::warn!(%error, root = %self.root.display(), "git cache: could not start the disk sweep");
        }
    }

    /// Body of the background sweep, entered with [`DiskTier::sweeping`] claimed: sweep until no
    /// request is pending, then release the claim. A request made between the last check and the
    /// release finds the claim still held and starts nothing, so after releasing it the pending flag
    /// is checked again and the claim taken back to serve it.
    fn run_sweeps(&self) {
        loop {
            {
                let _running = SweepRunning(&self.sweeping);
                while self.pending.swap(false, Ordering::SeqCst) {
                    self.sweep(SystemTime::now());
                }
            }
            if !self.reclaim_for_pending() {
                return;
            }
        }
    }

    /// Take the sweep claim back for a request left pending after it was released; `false` when
    /// none is pending or another sweep has taken the claim, which then serves the request.
    fn reclaim_for_pending(&self) -> bool {
        self.pending.load(Ordering::SeqCst) && !self.sweeping.swap(true, Ordering::SeqCst)
    }

    /// Sweep every subdirectory on the calling thread, aging temp files against `now`, and reset
    /// each running total to the bytes the sweep kept plus what was written while it ran.
    pub(crate) fn sweep(&self, now: SystemTime) {
        for sub in Subdir::ALL {
            let slot = &self.tracked[sub.slot()];
            let before = slot.load(Ordering::Acquire);
            let kept = evict_subdir(&self.dir(sub), self.max_bytes, now);
            let _ = slot.try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_sub(before).saturating_add(kept))
            });
        }
    }

    /// Zero every running total, for a cache whose directories were just emptied.
    pub(crate) fn reset(&self) {
        for slot in &self.tracked {
            slot.store(0, Ordering::Release);
        }
    }

    /// The running byte total of `sub`.
    #[cfg(test)]
    pub(crate) fn tracked(&self, sub: Subdir) -> u64 {
        self.tracked[sub.slot()].load(Ordering::Acquire)
    }

    /// Whether a background sweep is running.
    #[cfg(test)]
    pub(crate) fn is_sweeping(&self) -> bool {
        self.sweeping.load(Ordering::Acquire)
    }

    /// Block until the running background sweep, if any, has finished.
    #[cfg(test)]
    pub(crate) fn wait_for_sweep(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while self.is_sweeping() {
            assert!(
                std::time::Instant::now() < deadline,
                "the background sweep never finished"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Clears [`DiskTier::sweeping`] when the sweep ends, a panicking one included, so a failed sweep
/// does not stop every later one.
struct SweepRunning<'a>(&'a AtomicBool);

impl Drop for SweepRunning<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The bytes a sweep over `max_bytes` evicts down to.
fn low_water(max_bytes: u64) -> u64 {
    max_bytes - max_bytes / SWEEP_HEADROOM_DIVISOR
}

/// Sweep one directory: remove its stale temp files, and when its payloads exceed `max_bytes`
/// remove the oldest until they are at most [`low_water`]. Returns the payload bytes kept.
fn evict_subdir(dir: &Path, max_bytes: u64, now: SystemTime) -> u64 {
    let mut payloads = list_payloads(dir, now);
    let total = payloads.iter().map(|(_, size, _)| size).sum::<u64>();
    if max_bytes == 0 || total <= max_bytes {
        return total;
    }
    payloads.sort_by_key(|(_, _, mtime)| *mtime);
    let target = low_water(max_bytes);
    let mut kept = total;
    for (path, size, _) in payloads {
        if kept <= target {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            kept = kept.saturating_sub(size);
        }
    }
    kept
}

/// `(path, size, mtime)` of every payload in `dir`. A `*.tmp` file is not a payload: one past
/// [`STALE_TMP_AGE`] at `now` is removed, a younger one may be in flight and is left alone.
fn list_payloads(dir: &Path, now: SystemTime) -> Vec<(PathBuf, u64, SystemTime)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut payloads = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else { continue };
        if !metadata.is_file() {
            continue;
        }
        let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if path.extension().is_some_and(|ext| ext == "tmp") {
            if now.duration_since(mtime).is_ok_and(|age| age > STALE_TMP_AGE) {
                let _ = fs::remove_file(&path);
            }
            continue;
        }
        payloads.push((path, metadata.len(), mtime));
    }
    payloads
}

/// Monotonic per-process counter that makes temp-file names unique. PID alone collides when
/// two threads in the *same* process write the same cache key concurrently — both would pick
/// `<key>.msgpack.<pid>.tmp` and clobber each other mid-write. The counter splits them.
static ATOMIC_WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let seq = ATOMIC_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("msgpack.{}.{seq}.tmp", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tier over a fresh root with every subdirectory created.
    fn tier(max_bytes: u64) -> (tempfile::TempDir, Arc<DiskTier>) {
        let dir = tempfile::tempdir().unwrap();
        for sub in Subdir::ALL {
            fs::create_dir_all(dir.path().join(sub.name())).unwrap();
        }
        let tier = DiskTier::new(dir.path().to_path_buf(), max_bytes);
        (dir, tier)
    }

    fn set_mtime(path: &Path, mtime: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(mtime))
            .unwrap();
    }

    /// Concurrent same-process writers to the *same* destination key must not clobber each
    /// other's temp file mid-write: the final file is always one complete payload, never a
    /// torn/empty file. PID-only temp names broke this; the per-process sequence fixes it.
    #[test]
    fn concurrent_same_key_writes_never_tear() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("k.msgpack");
        let payloads: Vec<Vec<u8>> = (0..16u8).map(|i| vec![i; 4096]).collect();
        std::thread::scope(|scope| {
            for p in &payloads {
                let dest = dest.clone();
                scope.spawn(move || atomic_write(&dest, p).expect("atomic_write"));
            }
        });
        let got = fs::read(&dest).expect("dest exists");
        assert_eq!(got.len(), 4096, "final file must be a complete payload");
        let byte = got[0];
        assert!(
            got.iter().all(|&b| b == byte) && byte < 16,
            "final file must be exactly one writer's payload, not a mix"
        );
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must be renamed away, not orphaned");
    }

    /// Only `log/` was capped. `blame/` keys on the HEAD a blame was asked at and grew with every
    /// HEAD move, and a writer killed between write and rename left its temp file for good.
    #[test]
    fn every_subdirectory_is_capped_and_stale_temp_files_go() {
        let (dir, tier) = tier(1000);
        let root = dir.path();
        let old = SystemTime::now() - Duration::from_secs(3 * 3600);
        for sub in Subdir::ALL {
            let oldest = root.join(sub.name()).join("old.msgpack");
            fs::write(&oldest, vec![0u8; 600]).unwrap();
            set_mtime(&oldest, old);
            fs::write(root.join(sub.name()).join("new.msgpack"), vec![0u8; 600]).unwrap();
            let stale_tmp = root.join(sub.name()).join("x.msgpack.1.0.tmp");
            fs::write(&stale_tmp, b"torn").unwrap();
            set_mtime(&stale_tmp, old);
            fs::write(root.join(sub.name()).join("y.msgpack.1.1.tmp"), b"in flight").unwrap();
        }

        tier.sweep(SystemTime::now());

        for sub in Subdir::ALL {
            let name = sub.name();
            let dir = root.join(name);
            assert!(
                !dir.join("old.msgpack").exists(),
                "{name}: the oldest payload is evicted"
            );
            assert!(dir.join("new.msgpack").exists(), "{name}: the newest stays");
            assert!(
                !dir.join("x.msgpack.1.0.tmp").exists(),
                "{name}: a stale temp file goes"
            );
            assert!(
                dir.join("y.msgpack.1.1.tmp").exists(),
                "{name}: a fresh one may be in flight"
            );
            assert_eq!(tier.tracked(sub), 600, "{name}: the total is what the sweep kept");
        }
    }

    /// A temp file's age is measured against the sweep's clock, so the same file is in flight at
    /// one instant and abandoned past [`STALE_TMP_AGE`].
    #[test]
    fn temp_files_age_against_the_sweep_clock() {
        let (dir, tier) = tier(0);
        let tmp = dir.path().join("log").join("k.msgpack.1.0.tmp");
        fs::write(&tmp, b"partial").unwrap();
        let written = fs::metadata(&tmp).unwrap().modified().unwrap();

        tier.sweep(written + STALE_TMP_AGE);
        assert!(
            tmp.exists(),
            "a temp file exactly at the age limit may still be in flight"
        );

        tier.sweep(written + STALE_TMP_AGE + Duration::from_secs(1));
        assert!(!tmp.exists(), "a temp file past the age limit is abandoned");
    }

    /// The disk directories were bounded only when a stack opened, so a daemon hosting one for days
    /// grew them without limit. The write that crosses the budget now evicts in the background,
    /// down to the low-water mark, oldest first.
    #[test]
    fn a_write_past_the_budget_evicts_while_the_tier_stays_open() {
        let (dir, tier) = tier(1000);
        tier.sweep(SystemTime::now());
        let log = dir.path().join("log");
        let base = SystemTime::now() - Duration::from_secs(3600);
        for i in 0..3u64 {
            let path = log.join(format!("{i}.msgpack"));
            tier.write(Subdir::Log, &path, &[0u8; 300]);
            set_mtime(&path, base + Duration::from_secs(i));
        }
        assert_eq!(tier.tracked(Subdir::Log), 900);
        assert!(!tier.is_sweeping(), "a write within budget starts no sweep");

        tier.write(Subdir::Log, &log.join("3.msgpack"), &[0u8; 300]);
        tier.wait_for_sweep();

        let mut left: Vec<String> = fs::read_dir(&log)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["1.msgpack", "2.msgpack", "3.msgpack"]);
        assert_eq!(tier.tracked(Subdir::Log), 900);
    }

    /// A sweep that had already passed a directory when a write took it over budget never came back
    /// to it, and the write started nothing because a sweep was running, so the directory stayed
    /// over budget until a later write. The running sweep now serves that write before it ends.
    #[test]
    fn a_write_during_a_sweep_is_evicted_before_the_sweep_ends() {
        let (dir, tier) = tier(1000);
        let log = dir.path().join("log");
        let base = SystemTime::now() - Duration::from_secs(3600);
        tier.sweeping.store(true, Ordering::SeqCst);
        for i in 0..4u64 {
            let path = log.join(format!("{i}.msgpack"));
            tier.write(Subdir::Log, &path, &[0u8; 300]);
            set_mtime(&path, base + Duration::from_secs(i));
        }
        assert_eq!(
            fs::read_dir(&log).unwrap().count(),
            4,
            "the running sweep evicts, not the write"
        );

        tier.run_sweeps();

        let mut left: Vec<String> = fs::read_dir(&log)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["1.msgpack", "2.msgpack", "3.msgpack"]);
        assert_eq!(tier.tracked(Subdir::Log), 900);
        assert!(!tier.is_sweeping());
        assert!(!tier.pending.load(Ordering::SeqCst));
    }

    /// A request left pending after the claim was released takes the claim back, unless another
    /// sweep took it first; with nothing pending the sweep ends.
    #[test]
    fn a_request_pending_after_the_release_reclaims_the_sweep() {
        let (_dir, tier) = tier(1000);
        assert!(!tier.reclaim_for_pending(), "nothing pending");
        assert!(!tier.is_sweeping());

        tier.pending.store(true, Ordering::SeqCst);
        assert!(tier.reclaim_for_pending(), "a pending request takes the claim back");
        assert!(tier.is_sweeping());

        assert!(!tier.reclaim_for_pending(), "a claim another sweep holds is left to it");
    }

    /// A budget of `0` disables eviction however much is written.
    #[test]
    fn a_zero_budget_never_evicts() {
        let (dir, tier) = tier(0);
        let log = dir.path().join("log");
        for i in 0..4 {
            tier.write(Subdir::Log, &log.join(format!("{i}.msgpack")), &[0u8; 4096]);
        }
        assert!(!tier.is_sweeping());
        tier.sweep(SystemTime::now());
        assert_eq!(fs::read_dir(&log).unwrap().count(), 4);
        assert_eq!(tier.tracked(Subdir::Log), 4 * 4096);
    }

    /// The low-water mark leaves a tenth of the budget free, so the next write does not sweep again.
    #[test]
    fn low_water_leaves_a_tenth_of_the_budget_free() {
        assert_eq!(low_water(1000), 900);
        assert_eq!(low_water(LOG_CACHE_DEFAULT_MAX_BYTES), 241_591_911);
        assert_eq!(low_water(9), 9);
    }
}
