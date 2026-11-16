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
//! 2. **GC health state** ([`persist_gc_state`] / [`read_gc_state`]). Every attempt records its
//!    status in `gc-state.json`, including starvation and failures, and `cache_stats` surfaces it
//!    so a GC that silently stops running is observable instead of a 116 GB surprise.
//!
//! Split into its own module (mirroring `store_gc_workspace.rs`) to keep `store_gc.rs` under the
//! module size cap; the entry points are re-exported from [`crate::store_gc`].

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::store::{
    CACHE_DIR, WORKSPACE_MARKER_FILE, WORKSPACES_DIR, acquire_lock, cache_root, global_blobs_dir, read_workspace_marker,
};
use crate::store_gc::{GcError, GcReport, dir_size, gc_global_blobs_in, read_dir};
use crate::store_gc_protect::{ProtectedWorkspace, ProtectionReason, cache_grace};

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

/// How much older than the probe a blob must be before the sweep after an eviction takes it for
/// one the eviction orphaned: file systems that keep whole-second mtimes round a blob written just
/// after the probe to just before it.
const PROBE_MTIME_SLACK: Duration = Duration::from_secs(if cfg!(test) { 0 } else { 2 });

/// The grace the sweep after an eviction gives unreferenced blobs. Budget eviction runs holding
/// the daemon's blob-GC gate, so blobs the eviction orphaned can go at once without racing a scan
/// the daemon runs. A scan in another process is outside that gate, and its fresh blobs are in no
/// file map yet:
///
/// - while another process holds a workspace's writer lock, the ordinary grace period applies;
/// - otherwise only blobs older than `probed`, the instant before the locks were looked at, go.
///   Whatever a scan that started since has written is younger than that.
fn orphan_grace(workspaces_dir: &Path, probed: SystemTime) -> Duration {
    let foreign_writer = std::fs::read_dir(workspaces_dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| match crate::store::probe_writer_lock(&entry.path()) {
                crate::store::WriterProbe::Free => false,
                crate::store::WriterProbe::Held { holder } => holder.is_none_or(|meta| meta.pid != std::process::id()),
            })
    });
    if foreign_writer {
        crate::store_gc::blob_gc_grace()
    } else {
        probed.elapsed().unwrap_or(Duration::ZERO) + PROBE_MTIME_SLACK
    }
}

/// Workspace directory containing Lance tables. Kept feature-independent because budget cleanup
/// may inspect caches written by a richer binary than the one currently running.
const LANCE_WORKSPACE_DIR: &str = "lance";

/// Filename of the persisted last-GC state, in the machine-global cache root (next to `blobs/`
/// and `workspaces/`).
pub const GC_STATE_FILE: &str = "gc-state.json";

/// Operational outcome of the most recent GC attempt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GcStatus {
    /// The sweep completed and the cache is within its configured budget.
    #[default]
    Completed,
    /// The sweep completed, but protected or unavailable cache content kept it over budget.
    OverBudget,
    /// A rescan held the global GC lock for the full bounded wait.
    Starved,
    /// The sweep failed for another reason.
    Failed,
}

/// What the most recent destructive sweep did. Persisted as JSON by [`persist_gc_state`] and
/// surfaced by `cache_stats` so operators (and `/bm-stats` / doctor) can see whether GC is
/// actually running — the missing observability that let the starved-GC incident go unnoticed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GcState {
    /// When the most recent sweep finished (seconds since the Unix epoch); `0` if none completed.
    pub at_epoch_secs: u64,
    /// When the most recent attempt finished, including failed and starved attempts.
    #[serde(default)]
    pub last_attempt_epoch_secs: u64,
    /// Operational outcome of the most recent attempt.
    #[serde(default)]
    pub status: GcStatus,
    /// Actionable diagnosis for a degraded attempt.
    #[serde(default)]
    pub detail: Option<String>,
    /// Consecutive attempts that starved, failed, or ended over budget.
    #[serde(default)]
    pub consecutive_degraded_cycles: u32,
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
    /// Cache budget applied by the most recent completed sweep.
    #[serde(default)]
    pub cache_budget_bytes: Option<u64>,
    /// Measured cache footprint after the most recent completed budget pass.
    #[serde(default)]
    pub cache_bytes_after: Option<u64>,
    /// Hot workspaces evicted after cold candidates were exhausted.
    #[serde(default)]
    pub hot_workspaces_evicted: usize,
    /// Workspaces budget enforcement skipped because they were locked.
    #[serde(default)]
    pub locked_workspaces_skipped: usize,
    /// Workspaces the budget pass refused to evict (locked, most recently used, or inside the
    /// post-build grace window) while the cache was over budget. Non-empty with
    /// [`GcStatus::OverBudget`] means the budget is knowingly exceeded to keep a live index.
    #[serde(default)]
    pub protected_workspaces: Vec<ProtectedWorkspace>,
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
    /// Workspaces left alone because they are locked, most recently used, or freshly built. Only
    /// workspaces the pass actually reached while over budget are listed.
    pub protected: Vec<ProtectedWorkspace>,
}

struct EvictionCandidate {
    activity: SystemTime,
    hot: bool,
    has_views: bool,
    protected: Option<ProtectionReason>,
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
    let now = epoch_secs();
    let over_budget = report
        .cache_budget_bytes
        .zip(report.cache_bytes_after)
        .is_some_and(|(budget, after)| after > budget);
    let previous_degraded_cycles = read_gc_state_at(path)
        .map(|state| state.consecutive_degraded_cycles)
        .unwrap_or(0);
    let state = GcState {
        at_epoch_secs: now,
        last_attempt_epoch_secs: now,
        status: if over_budget {
            GcStatus::OverBudget
        } else {
            GcStatus::Completed
        },
        detail: over_budget.then(|| over_budget_detail(report)),
        consecutive_degraded_cycles: if over_budget {
            previous_degraded_cycles.saturating_add(1)
        } else {
            0
        },
        scanned: report.scanned,
        removed: report.removed,
        bytes_freed: report.bytes_freed,
        workspaces_reaped: report.workspaces_reaped,
        workspace_bytes_freed: report.workspace_bytes_freed,
        workspaces_evicted: report.workspaces_evicted,
        evicted_bytes_freed: report.evicted_bytes_freed,
        cache_budget_bytes: report.cache_budget_bytes,
        cache_bytes_after: report.cache_bytes_after,
        hot_workspaces_evicted: report.hot_workspaces_evicted,
        locked_workspaces_skipped: report.locked_workspaces_skipped,
        protected_workspaces: report.protected_workspaces.clone(),
    };
    write_gc_state(path, &state);
}

/// Persist a failed or starved GC attempt while preserving the last completed sweep counters.
pub fn persist_gc_error(error: &GcError) {
    persist_gc_error_at(&gc_state_path(), error);
}

fn persist_gc_error_at(path: &Path, error: &GcError) {
    let mut state = read_gc_state_at(path).unwrap_or_default();
    state.last_attempt_epoch_secs = epoch_secs();
    state.status = match error {
        GcError::Starved(_) => GcStatus::Starved,
        _ => GcStatus::Failed,
    };
    state.detail = Some(error.to_string());
    state.consecutive_degraded_cycles = state.consecutive_degraded_cycles.saturating_add(1);
    write_gc_state(path, &state);
}

fn over_budget_detail(report: &GcReport) -> String {
    let budget = report.cache_budget_bytes.unwrap_or(0);
    let after = report.cache_bytes_after.unwrap_or(budget);
    let protected: u64 = report.protected_workspaces.iter().map(|ws| ws.bytes).sum();
    format!(
        "cache remains {} bytes over budget after enforcement; {} workspace(s) holding {protected} bytes were \
         protected (locked, most recently used, or built within the grace window) and kept; retrying next cycle",
        after.saturating_sub(budget),
        report.protected_workspaces.len().max(report.locked_workspaces_skipped),
    )
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn write_gc_state(path: &Path, state: &GcState) {
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
    enforce_cache_budget_at(
        &cache_root().join(CACHE_DIR).join(WORKSPACES_DIR),
        &crate::store_layout::global_blobs_root(),
        &global_blobs_dir(),
        crate::extract::SCHEMA_VER,
        budget_bytes,
        BUDGET_EVICTION_HOT_FLOOR,
        cache_grace(),
    )
}

/// [`enforce_cache_budget`] against a per-fixture cache whose blob store is one flat directory.
#[cfg(test)]
pub(crate) fn enforce_cache_budget_in(
    workspaces_dir: &Path,
    blobs_dir: &Path,
    budget_bytes: u64,
    hot_floor: Duration,
    grace: Duration,
) -> Result<EvictReport, GcError> {
    enforce_cache_budget_at(
        workspaces_dir,
        blobs_dir,
        blobs_dir,
        crate::extract::SCHEMA_VER,
        budget_bytes,
        hot_floor,
        grace,
    )
}

/// [`enforce_cache_budget`] against explicit directories. The footprint counts what this binary
/// can free: the workspaces and everything under `blobs_root` except the namespaces of a blob
/// schema newer than `schema` (see [`cache_footprint`]). Blobs an eviction orphans are reclaimed
/// in `blobs_dir`, the namespace the evicted workspaces of this version referenced.
pub(crate) fn enforce_cache_budget_at(
    workspaces_dir: &Path,
    blobs_root: &Path,
    blobs_dir: &Path,
    schema: u16,
    budget_bytes: u64,
    hot_floor: Duration,
    grace: Duration,
) -> Result<EvictReport, GcError> {
    let total = cache_footprint(workspaces_dir, blobs_root, schema)?;
    let mut report = EvictReport {
        total_bytes_before: total,
        total_bytes_after: total,
        ..EvictReport::default()
    };
    if report.total_bytes_before <= budget_bytes {
        return Ok(report);
    }

    for candidate in eviction_candidates(workspaces_dir, hot_floor, grace)? {
        if report.total_bytes_after <= budget_bytes {
            break;
        }
        if let Some(reason) = candidate.protected {
            report.protected.push(protected_entry(&candidate.dir, reason));
            continue;
        }
        let bytes_before_candidate = report.total_bytes_after;
        let size = match evict_workspace(&candidate.dir)? {
            Eviction::Done(size) => size,
            Eviction::Refused(reason) => {
                report.locked_skipped += usize::from(reason == ProtectionReason::Locked);
                report.protected.push(protected_entry(&candidate.dir, reason));
                continue;
            }
        };
        let probed = SystemTime::now();
        let reclaimed = gc_global_blobs_in(workspaces_dir, blobs_dir, orphan_grace(workspaces_dir, probed))?;
        report.blobs_removed += reclaimed.removed;
        report.blob_bytes_freed += reclaimed.bytes_freed;
        report.total_bytes_after = cache_footprint(workspaces_dir, blobs_root, schema)?;
        let measured_freed = bytes_before_candidate.saturating_sub(report.total_bytes_after);
        if size > 0 || reclaimed.bytes_freed > 0 {
            report.evicted += 1;
            report.bytes_freed += size;
            report.hot_evicted += usize::from(candidate.hot);
        }
        tracing::info!(
            workspace = %candidate.dir.display(),
            workspace_bytes = size,
            blob_bytes = reclaimed.bytes_freed,
            total_bytes = report.total_bytes_after,
            measured_bytes_freed = measured_freed,
            "evicted workspace cache to enforce the size budget"
        );
    }
    if report.total_bytes_after > budget_bytes && !report.protected.is_empty() {
        tracing::warn!(
            total_bytes = report.total_bytes_after,
            budget_bytes,
            protected = report.protected.len(),
            protected_bytes = report.protected.iter().map(|ws| ws.bytes).sum::<u64>(),
            "cache exceeds its size budget but the remaining workspaces are in use (locked, most recently \
             used, built within the grace window, or written by a newer basemind); keeping them. Raise \
             BASEMIND_CACHE_BUDGET_MB to silence this"
        );
    }
    Ok(report)
}

fn protected_entry(dir: &Path, reason: ProtectionReason) -> ProtectedWorkspace {
    ProtectedWorkspace {
        key: dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        root: read_workspace_marker(dir).map(|marker| marker.root.display().to_string()),
        bytes: dir_size(dir).unwrap_or(0),
        reason,
    }
}

fn eviction_candidates(
    workspaces_dir: &Path,
    hot_floor: Duration,
    grace: Duration,
) -> Result<Vec<EvictionCandidate>, GcError> {
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
        let (activity, has_views) = workspace_last_activity(&dir);
        let idle = now.duration_since(activity).unwrap_or(Duration::ZERO);
        // A workspace with no view left holds nothing worth protecting.
        let protected = (has_views && idle < grace).then_some(ProtectionReason::RecentlyBuilt);
        candidates.push(EvictionCandidate {
            activity,
            hot: idle < hot_floor,
            has_views,
            protected,
            dir,
        });
    }
    // The most recently used workspace across every worktree of every repo is always protected,
    // however old it is and whatever the grace window says.
    if let Some(mru) = candidates
        .iter()
        .filter(|candidate| candidate.has_views)
        .max_by_key(|candidate| candidate.activity)
        .map(|candidate| candidate.dir.clone())
    {
        for candidate in candidates.iter_mut().filter(|candidate| candidate.dir == mru) {
            candidate.protected = Some(ProtectionReason::MostRecentlyUsed);
        }
    }
    candidates.sort_by_key(|candidate| (candidate.hot, candidate.activity));
    Ok(candidates)
}

/// What became of a workspace picked for eviction.
enum Eviction {
    /// Its rebuildable state is gone; the bytes that freed.
    Done(u64),
    /// It was left whole.
    Refused(ProtectionReason),
}

fn evict_workspace(dir: &Path) -> Result<Eviction, GcError> {
    let Ok(lock) = acquire_lock(dir) else {
        tracing::debug!(workspace = %dir.display(), "over-budget workspace is locked; skipping");
        return Ok(Eviction::Refused(ProtectionReason::Locked));
    };
    if holds_a_newer_view(dir) {
        tracing::debug!(workspace = %dir.display(), "over-budget workspace was written by a newer basemind; skipping");
        return Ok(Eviction::Refused(ProtectionReason::NewerVersion));
    }
    let before = dir_size(dir)?;
    for path in rebuildable_workspace_paths(dir) {
        remove_cache_path(&path)?;
    }
    let after = dir_size(dir)?;
    drop(lock);
    Ok(Eviction::Done(before.saturating_sub(after)))
}

/// Whether a view of this workspace was written by a newer basemind. Such a view is that
/// version's live index over blobs this binary cannot read, not cache this one may rebuild.
fn holds_a_newer_view(workspace: &Path) -> bool {
    let views = workspace.join(crate::store::VIEWS_DIR);
    std::fs::read_dir(views).is_ok_and(|entries| {
        entries.flatten().any(|view| {
            matches!(
                crate::store::read_index(&view.path()),
                Err(crate::store::StoreError::NewerSchema { .. })
            )
        })
    })
}

/// Derived state that can be rebuilt from source. Stable identity, the workspace marker and the
/// per-repo-group `repos/<group>/` (the memory notes and their vector stores, a sibling of
/// `workspaces/`, so never under `workspace`) are intentionally absent: budget enforcement must
/// never rotate an agent's identity or delete user-authored memory.
fn rebuildable_workspace_paths(workspace: &Path) -> [PathBuf; 8] {
    let lance = workspace.join(LANCE_WORKSPACE_DIR);
    [
        workspace.join(crate::store::VIEWS_DIR),
        workspace.join("git-cache"),
        workspace.join(crate::git_history::GIT_HISTORY_DIR),
        workspace.join("status.json"),
        workspace.join("telemetry.jsonl"),
        lance.join("documents.lance"),
        lance.join("code_chunks.lance"),
        lance.join("doc_links.lance"),
    ]
}

fn remove_cache_path(path: &Path) -> Result<(), GcError> {
    let result = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else if path.exists() {
        std::fs::remove_file(path)
    } else {
        return Ok(());
    };
    result.map_err(|source| GcError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// The bytes the budget is held against: the workspaces, and the blob store without the
/// namespaces of a schema newer than `schema`. Those belong to a newer basemind sharing the cache.
/// This binary never deletes them, so counting them would have it evict workspaces every cycle to
/// make up for bytes no eviction can free.
fn cache_footprint(workspaces_dir: &Path, blobs_root: &Path, schema: u16) -> Result<u64, GcError> {
    let mut total = dir_size_or_zero(workspaces_dir)? + dir_size_or_zero(blobs_root)?;
    if blobs_root.exists() {
        for entry in read_dir(blobs_root)?.flatten() {
            let newer = entry
                .file_name()
                .to_str()
                .and_then(crate::store_gc_namespaces::namespace_schema)
                .is_some_and(|found| found > schema);
            if newer && entry.path().is_dir() {
                total = total.saturating_sub(dir_size(&entry.path())?);
            }
        }
    }
    Ok(total)
}

/// Newest mtime among the files a scan always rewrites (`workspace.json`, `status.json`, each view's
/// `index.msgpack`), plus whether any view index exists. The dir's own mtime is only the fallback
/// when none of those exist — the dir inode is touched by lock-file churn (including GC's own
/// probes), so using it as a floor would make every workspace look perpetually hot. A dir with
/// neither reads as epoch ⇒ maximally cold (an unreadable cache dir is the best eviction candidate,
/// and evicting a cache is always safe).
fn workspace_last_activity(workspace_dir: &Path) -> (SystemTime, bool) {
    let mut newest: Option<SystemTime> = None;
    let mut consider = |path: &Path| -> bool {
        let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
            return false;
        };
        if newest.is_none_or(|current| modified > current) {
            newest = Some(modified);
        }
        true
    };
    consider(&workspace_dir.join(WORKSPACE_MARKER_FILE));
    consider(&workspace_dir.join(crate::store::STATUS_SIDECAR_FILE));
    let mut has_views = false;
    let views = workspace_dir.join(crate::store::VIEWS_DIR);
    if let Ok(entries) = std::fs::read_dir(&views) {
        for entry in entries.flatten() {
            has_views |= consider(&entry.path().join(crate::store::INDEX_FILE));
        }
    }
    let activity = newest.unwrap_or_else(|| {
        std::fs::metadata(workspace_dir)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    });
    (activity, has_views)
}

/// [`dir_size`] that treats a missing directory as empty (the budget check runs against dirs
/// that may not exist yet on a fresh machine).
fn dir_size_or_zero(dir: &Path) -> Result<u64, GcError> {
    if dir.exists() { dir_size(dir) } else { Ok(0) }
}

#[cfg(test)]
#[path = "store_gc_budget_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "store_gc_protect_tests.rs"]
mod protect_tests;
