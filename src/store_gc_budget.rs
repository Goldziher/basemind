//! Cache size-budget enforcement + last-GC state persistence for the machine-global cache.
//!
//! Two responsibilities, both born from the 116 GB incident where the daemon's hourly sweep was
//! starved behind a runaway rescan and the cache grew unbounded and invisibly:
//!
//! 1. **Size budget** ([`enforce_cache_budget`]). The blob store + workspace indexes are a
//!    rebuildable cache, so when their combined footprint exceeds a byte budget the coldest
//!    workspace dirs are evicted (their blobs orphan and the next sweep reclaims them). This is
//!    the backstop the reference-counting GC lacks: reaping only ever removes *orphans*, so a
//!    machine that accumulates live-but-idle workspaces still grows without bound.
//! 2. **Last-GC state** ([`persist_gc_state`] / [`read_gc_state`]). Every destructive sweep
//!    records what it did to `gc-state.json` in the cache root, and `cache_stats` surfaces it —
//!    so a GC that silently stops running is observable instead of a 116 GB surprise.
//!
//! Split into its own module (mirroring `store_gc_workspace.rs`) to keep `store_gc.rs` under the
//! module size cap; the entry points are re-exported from [`crate::store_gc`].

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::store::{CACHE_DIR, WORKSPACE_MARKER_FILE, WORKSPACES_DIR, acquire_lock, cache_root, global_blobs_dir};
use crate::store_gc::{GcError, GcReport, dir_size, gc_global_blobs_in, read_dir};

/// Environment variable holding the cache byte budget in MiB. Unset ⇒
/// [`DEFAULT_CACHE_BUDGET_MIB`]; `0` ⇒ unlimited (budget enforcement disabled).
pub const CACHE_BUDGET_ENV: &str = "BASEMIND_CACHE_BUDGET_MB";

/// Default cache budget when [`CACHE_BUDGET_ENV`] is unset: 20 GiB. Generous enough that a
/// handful of monorepos never trip it; small enough that an unbounded-growth regression is
/// contained long before it threatens the disk.
const DEFAULT_CACHE_BUDGET_MIB: u64 = 20 * 1024;

/// Workspaces touched more recently than this are considered only after every cold candidate.
/// Evicting a hot workspace risks an immediate rescan, but an absolute veto would let the cache
/// remain unbounded when hot workspaces alone exceed the configured budget.
const BUDGET_EVICTION_HOT_FLOOR: Duration = Duration::from_secs(24 * 60 * 60);

/// Budget eviction runs under the daemon's global GC write lock, so blobs orphaned by a deliberate
/// workspace eviction can be reclaimed immediately without racing an in-flight scan.
const BUDGET_ORPHAN_GRACE: Duration = Duration::ZERO;

/// Filename of the persisted last-GC state, in the machine-global cache root (next to `blobs/`
/// and `workspaces/`).
pub const GC_STATE_FILE: &str = "gc-state.json";

/// What the most recent destructive sweep did. Persisted as JSON by [`persist_gc_state`] and
/// surfaced by `cache_stats` so operators (and `/bm-stats` / doctor) can see whether GC is
/// actually running — the missing observability that let the starved-GC incident go unnoticed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcState {
    /// When the sweep finished (seconds since the Unix epoch).
    pub at_epoch_secs: u64,
    /// Blob files inspected.
    pub scanned: usize,
    /// Orphan blob files removed.
    pub removed: usize,
    /// Blob bytes reclaimed.
    pub bytes_freed: u64,
    /// Orphaned workspace dirs reaped (worktree root gone).
    pub workspaces_reaped: usize,
    /// Bytes reclaimed by reaping those dirs.
    pub workspace_bytes_freed: u64,
    /// Cold workspace dirs evicted by budget enforcement.
    #[serde(default)]
    pub workspaces_evicted: usize,
    /// Bytes reclaimed by those evictions.
    #[serde(default)]
    pub evicted_bytes_freed: u64,
}

/// Result of one budget-enforcement pass.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EvictReport {
    /// Combined cache footprint (workspaces + blobs) measured before eviction.
    pub total_bytes_before: u64,
    /// Combined cache footprint measured after the final eviction/reclamation step.
    pub total_bytes_after: u64,
    /// Workspace dirs evicted.
    pub evicted: usize,
    /// Bytes those dirs occupied (stat'd before deletion).
    pub bytes_freed: u64,
    /// Blob files reclaimed after workspace eviction removed their final reference.
    pub blobs_removed: usize,
    /// Bytes reclaimed from those orphan blobs.
    pub blob_bytes_freed: u64,
    /// Hot workspaces evicted after cold candidates could not satisfy the budget.
    pub hot_evicted: usize,
    /// Workspaces skipped because another process held their lock.
    pub locked_skipped: usize,
}

struct EvictionCandidate {
    activity: SystemTime,
    hot: bool,
    dir: PathBuf,
}

/// The configured cache byte budget: [`CACHE_BUDGET_ENV`] in MiB, defaulting to
/// [`DEFAULT_CACHE_BUDGET_MIB`]; `0` (or an unparseable value, which is logged) disables
/// enforcement and returns `None`.
pub fn cache_budget_bytes() -> Option<u64> {
    let mib = match std::env::var(CACHE_BUDGET_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(mib) => mib,
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    "{CACHE_BUDGET_ENV} is not a number; disabling cache budget enforcement"
                );
                return None;
            }
        },
        Err(_) => DEFAULT_CACHE_BUDGET_MIB,
    };
    (mib > 0).then_some(mib * 1024 * 1024)
}

/// Path of the persisted last-GC state in the machine-global cache root.
pub fn gc_state_path() -> PathBuf {
    cache_root().join(CACHE_DIR).join(GC_STATE_FILE)
}

/// Record what a destructive sweep did (best-effort: persistence failure is logged, never
/// propagated — losing a telemetry write must not fail the sweep that just succeeded).
pub fn persist_gc_state(report: &GcReport) {
    persist_gc_state_at(&gc_state_path(), report);
}

/// [`persist_gc_state`] against an explicit path (tests pass a temp file).
pub(crate) fn persist_gc_state_at(path: &Path, report: &GcReport) {
    let state = GcState {
        at_epoch_secs: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        scanned: report.scanned,
        removed: report.removed,
        bytes_freed: report.bytes_freed,
        workspaces_reaped: report.workspaces_reaped,
        workspace_bytes_freed: report.workspace_bytes_freed,
        workspaces_evicted: report.workspaces_evicted,
        evicted_bytes_freed: report.evicted_bytes_freed,
    };
    let write = serde_json::to_vec_pretty(&state)
        .map_err(std::io::Error::other)
        .and_then(|bytes| {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, bytes)
        });
    if let Err(error) = write {
        tracing::warn!(%error, path = %path.display(), "failed to persist gc-state.json");
    }
}

/// Read the persisted last-GC state, if any. `None` covers both "no sweep has ever run" and an
/// unreadable/corrupt file (logged at debug — the state is advisory).
pub fn read_gc_state() -> Option<GcState> {
    read_gc_state_at(&gc_state_path())
}

/// [`read_gc_state`] against an explicit path (tests, and `cache_stats` which derives the path
/// from its injected blob dir).
pub(crate) fn read_gc_state_at(path: &Path) -> Option<GcState> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(state) => Some(state),
        Err(error) => {
            tracing::debug!(%error, path = %path.display(), "gc-state.json unreadable; ignoring");
            None
        }
    }
}

/// Enforce the cache byte budget against the machine-global cache: when workspaces + blobs
/// exceed `budget_bytes`, evict the coldest workspace dirs, then hot dirs only if necessary.
/// Locked workspaces remain protected. Each eviction immediately reclaims blobs that lost their
/// final reference and remeasures the real footprint before selecting another victim.
pub fn enforce_cache_budget(budget_bytes: u64) -> Result<EvictReport, GcError> {
    enforce_cache_budget_in(
        &cache_root().join(CACHE_DIR).join(WORKSPACES_DIR),
        &global_blobs_dir(),
        budget_bytes,
        BUDGET_EVICTION_HOT_FLOOR,
    )
}

/// [`enforce_cache_budget`] against explicit directories (tests pass a per-fixture cache and a
/// zero hot floor).
pub(crate) fn enforce_cache_budget_in(
    workspaces_dir: &Path,
    blobs_dir: &Path,
    budget_bytes: u64,
    hot_floor: Duration,
) -> Result<EvictReport, GcError> {
    let total = cache_footprint(workspaces_dir, blobs_dir)?;
    let mut report = EvictReport {
        total_bytes_before: total,
        total_bytes_after: total,
        ..EvictReport::default()
    };
    if report.total_bytes_before <= budget_bytes {
        return Ok(report);
    }

    for candidate in eviction_candidates(workspaces_dir, hot_floor)? {
        if report.total_bytes_after <= budget_bytes {
            break;
        }
        let Some(size) = evict_workspace(&candidate.dir)? else {
            report.locked_skipped += 1;
            continue;
        };
        let reclaimed = gc_global_blobs_in(workspaces_dir, blobs_dir, BUDGET_ORPHAN_GRACE)?;
        report.evicted += 1;
        report.bytes_freed += size;
        report.blobs_removed += reclaimed.removed;
        report.blob_bytes_freed += reclaimed.bytes_freed;
        report.hot_evicted += usize::from(candidate.hot);
        report.total_bytes_after = cache_footprint(workspaces_dir, blobs_dir)?;
        tracing::info!(
            workspace = %candidate.dir.display(),
            workspace_bytes = size,
            blob_bytes = reclaimed.bytes_freed,
            total_bytes = report.total_bytes_after,
            "evicted workspace cache to enforce the size budget"
        );
    }
    Ok(report)
}

fn eviction_candidates(workspaces_dir: &Path, hot_floor: Duration) -> Result<Vec<EvictionCandidate>, GcError> {
    let mut candidates = Vec::new();
    let now = SystemTime::now();
    if !workspaces_dir.exists() {
        return Ok(candidates);
    }
    for entry in read_dir(workspaces_dir)? {
        let entry = entry.map_err(|source| GcError::Io {
            path: workspaces_dir.to_path_buf(),
            source,
        })?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let activity = workspace_last_activity(&dir);
        let idle = now.duration_since(activity).unwrap_or(Duration::ZERO);
        candidates.push(EvictionCandidate {
            activity,
            hot: idle < hot_floor,
            dir,
        });
    }
    candidates.sort_by_key(|candidate| (candidate.hot, candidate.activity));
    Ok(candidates)
}

fn evict_workspace(dir: &Path) -> Result<Option<u64>, GcError> {
    let Ok(lock) = acquire_lock(dir) else {
        tracing::debug!(workspace = %dir.display(), "over-budget workspace is locked; skipping");
        return Ok(None);
    };
    let size = dir_size(dir)?;
    std::fs::remove_dir_all(dir).map_err(|source| GcError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    drop(lock);
    Ok(Some(size))
}

fn cache_footprint(workspaces_dir: &Path, blobs_dir: &Path) -> Result<u64, GcError> {
    Ok(dir_size_or_zero(workspaces_dir)? + dir_size_or_zero(blobs_dir)?)
}

/// Newest mtime among the files a scan always rewrites (`workspace.json`, each view's
/// `index.msgpack`). The dir's own mtime is only the fallback when none of those exist — the dir
/// inode is touched by lock-file churn (including GC's own probes), so using it as a floor would
/// make every workspace look perpetually hot. A dir with neither reads as epoch ⇒ maximally cold
/// (an unreadable cache dir is the best eviction candidate, and evicting a cache is always safe).
fn workspace_last_activity(workspace_dir: &Path) -> SystemTime {
    let mut newest: Option<SystemTime> = None;
    let mut consider = |path: &Path| {
        if let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified())
            && newest.is_none_or(|current| modified > current)
        {
            newest = Some(modified);
        }
    };
    consider(&workspace_dir.join(WORKSPACE_MARKER_FILE));
    let views = workspace_dir.join(crate::store::VIEWS_DIR);
    if let Ok(entries) = std::fs::read_dir(&views) {
        for entry in entries.flatten() {
            consider(&entry.path().join(crate::store::INDEX_FILE));
        }
    }
    newest.unwrap_or_else(|| {
        std::fs::metadata(workspace_dir)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    })
}

/// [`dir_size`] that treats a missing directory as empty (the budget check runs against dirs
/// that may not exist yet on a fresh machine).
fn dir_size_or_zero(dir: &Path) -> Result<u64, GcError> {
    if dir.exists() { dir_size(dir) } else { Ok(0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{FileEntry, INDEX_FILE, Index, VIEWS_DIR, ensure_workspace_marker};
    use std::fs;

    /// Seed `<workspaces>/<key>/` with a marker + a working view whose index references `stem`,
    /// then force every activity-bearing file's mtime to `age` ago.
    fn seed_workspace_aged(workspaces_dir: &Path, key: &str, stem: &str, age: Duration) -> PathBuf {
        let workspace_dir = workspaces_dir.join(key);
        let working = workspace_dir.join(VIEWS_DIR).join("working");
        fs::create_dir_all(&working).expect("mk workspace view");
        let mut index = Index::empty();
        index.files.insert(
            crate::path::RelPath::from("src/main.rs"),
            FileEntry {
                hash_hex: stem.to_string(),
                language: "rust".to_string(),
                size_bytes: 2,
                mtime: 0,
            },
        );
        fs::write(
            working.join(INDEX_FILE),
            rmp_serde::to_vec_named(&index).expect("encode index"),
        )
        .expect("write index");
        ensure_workspace_marker(&workspace_dir, workspaces_dir);

        let stamp = SystemTime::now() - age;
        for file in [workspace_dir.join(WORKSPACE_MARKER_FILE), working.join(INDEX_FILE)] {
            fs::File::options()
                .write(true)
                .open(&file)
                .and_then(|f| f.set_modified(stamp))
                .expect("age file");
        }
        workspace_dir
    }

    #[test]
    fn under_budget_is_a_no_op() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let blobs = tmp.path().join("blobs");
        fs::create_dir_all(&blobs).expect("mk blobs");
        let ws = seed_workspace_aged(&workspaces, "key-a", &"a".repeat(64), Duration::from_secs(9999));

        let report =
            enforce_cache_budget_in(&workspaces, &blobs, u64::MAX, Duration::ZERO).expect("enforce under budget");

        assert_eq!(report.evicted, 0, "under budget must evict nothing");
        assert_eq!(report.bytes_freed, 0);
        assert!(report.total_bytes_before > 0, "footprint is measured");
        assert_eq!(report.total_bytes_after, report.total_bytes_before);
        assert!(ws.exists(), "workspace untouched");
    }

    #[test]
    fn over_budget_prefers_a_cold_workspace_and_stops_before_a_hot_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let blobs = tmp.path().join("blobs");
        fs::create_dir_all(&blobs).expect("mk blobs");

        let cold = seed_workspace_aged(
            &workspaces,
            "key-cold",
            &"a".repeat(64),
            Duration::from_secs(10 * 24 * 3600),
        );
        let warm = seed_workspace_aged(&workspaces, "key-warm", &"b".repeat(64), Duration::from_secs(60));

        let total = dir_size(&workspaces).expect("size");
        let one_ws = dir_size(&cold).expect("size cold");
        let budget = total - one_ws / 2;

        let report =
            enforce_cache_budget_in(&workspaces, &blobs, budget, Duration::from_secs(24 * 3600)).expect("enforce");

        assert_eq!(report.evicted, 1, "one eviction suffices to reach the budget");
        assert_eq!(report.hot_evicted, 0, "the cold candidate is preferred");
        assert!(report.total_bytes_after <= budget, "the measured footprint converged");
        assert!(!cold.exists(), "the coldest workspace is the one evicted");
        assert!(warm.exists(), "the warmer workspace survives");
        assert!(report.bytes_freed >= one_ws, "freed bytes cover the evicted tree");
    }

    #[test]
    fn a_hot_workspace_is_evicted_when_it_is_the_only_way_to_converge() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let blobs = tmp.path().join("blobs");
        fs::create_dir_all(&blobs).expect("mk blobs");

        let hot = seed_workspace_aged(&workspaces, "key-hot", &"a".repeat(64), Duration::ZERO);

        let report = enforce_cache_budget_in(&workspaces, &blobs, 1, Duration::from_secs(24 * 3600))
            .expect("enforce with hot floor");

        assert_eq!(report.evicted, 1, "the hot floor is a preference, not a budget veto");
        assert_eq!(report.hot_evicted, 1);
        assert!(report.total_bytes_after <= 1);
        assert!(!hot.exists());
    }

    #[test]
    fn reclaiming_an_evicted_workspaces_blobs_prevents_an_extra_eviction() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let blobs = tmp.path().join("blobs");
        fs::create_dir_all(&blobs).expect("mk blobs");

        let cold_stem = "c".repeat(64);
        let cold = seed_workspace_aged(&workspaces, "key-cold", &cold_stem, Duration::from_secs(10 * 24 * 3600));
        let warm = seed_workspace_aged(
            &workspaces,
            "key-warm",
            &"d".repeat(64),
            Duration::from_secs(2 * 24 * 3600),
        );
        let cold_blob = blobs.join(format!("{cold_stem}.fm.msgpack"));
        fs::write(&cold_blob, vec![7_u8; 64 * 1024]).expect("write cold workspace blob");

        let total = dir_size(&workspaces).unwrap() + dir_size(&blobs).unwrap();
        let reclaimable = dir_size(&cold).unwrap() + fs::metadata(&cold_blob).unwrap().len();
        let budget = total - reclaimable + 1;

        let report = enforce_cache_budget_in(&workspaces, &blobs, budget, Duration::ZERO).expect("enforce");

        assert_eq!(
            report.evicted, 1,
            "blob reclamation should make a second eviction unnecessary"
        );
        assert_eq!(report.blobs_removed, 1);
        assert_eq!(report.blob_bytes_freed, 64 * 1024);
        assert!(report.total_bytes_after <= budget);
        assert!(!cold.exists());
        assert!(
            !cold_blob.exists(),
            "the evicted workspace's orphan blob is reclaimed in the same pass"
        );
        assert!(
            warm.exists(),
            "the warmer workspace survives once the measured footprint converges"
        );
    }

    #[test]
    fn a_locked_workspace_is_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspaces = tmp.path().join("workspaces");
        let blobs = tmp.path().join("blobs");
        fs::create_dir_all(&blobs).expect("mk blobs");

        let locked = seed_workspace_aged(&workspaces, "key-locked", &"a".repeat(64), Duration::from_secs(9999));
        let _held = acquire_lock(&locked).expect("hold the workspace lock");

        let report = enforce_cache_budget_in(&workspaces, &blobs, 1, Duration::ZERO).expect("enforce");

        assert_eq!(report.evicted, 0, "a locked workspace is never evicted");
        assert_eq!(report.locked_skipped, 1);
        assert!(report.total_bytes_after > 1, "the report exposes non-convergence");
        assert!(locked.exists());
    }

    #[test]
    fn gc_state_round_trips_and_tolerates_absence() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("cache").join(GC_STATE_FILE);

        assert!(read_gc_state_at(&path).is_none(), "absent state reads as None");

        let report = GcReport {
            scanned: 10,
            removed: 3,
            bytes_freed: 4096,
            workspaces_reaped: 1,
            workspace_bytes_freed: 2048,
            workspaces_evicted: 2,
            evicted_bytes_freed: 8192,
        };
        persist_gc_state_at(&path, &report);

        let state = read_gc_state_at(&path).expect("state persisted");
        assert!(state.at_epoch_secs > 0, "timestamp recorded");
        assert_eq!(state.removed, 3);
        assert_eq!(state.bytes_freed, 4096);
        assert_eq!(state.workspaces_reaped, 1);
        assert_eq!(state.workspaces_evicted, 2);
        assert_eq!(state.evicted_bytes_freed, 8192);

        fs::write(&path, b"{ not json").expect("corrupt");
        assert!(read_gc_state_at(&path).is_none(), "corrupt state degrades to None");
    }

    #[test]
    fn cache_budget_env_parses_default_zero_and_garbage() {
        const VAR: &str = CACHE_BUDGET_ENV;
        // SAFETY: test-only env mutation; no other thread in this test binary reads this var
        unsafe { std::env::remove_var(VAR) };
        assert_eq!(cache_budget_bytes(), Some(DEFAULT_CACHE_BUDGET_MIB * 1024 * 1024));
        unsafe { std::env::set_var(VAR, "0") };
        assert_eq!(cache_budget_bytes(), None, "0 disables enforcement");
        unsafe { std::env::set_var(VAR, "512") };
        assert_eq!(cache_budget_bytes(), Some(512 * 1024 * 1024));
        unsafe { std::env::set_var(VAR, "not-a-number") };
        assert_eq!(cache_budget_bytes(), None, "garbage disables enforcement (logged)");
        unsafe { std::env::remove_var(VAR) };
    }
}
