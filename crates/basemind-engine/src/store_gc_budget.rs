//! Cache size-budget enforcement + last-GC state persistence for the machine-global cache.
//!
//! Two responsibilities, both born from the 116 GB incident where the daemon's hourly sweep was
//! starved behind a runaway rescan and the cache grew unbounded and invisibly:
//!
//! 1. **Size budget** ([`enforce_cache_budget`]). The blob store, the workspaces and the group
//!    directories (`cache/groups/<group>/`: the code index and the document retrieval stores) are a
//!    rebuildable cache, so when their combined footprint exceeds a byte budget the per-view
//!    indexes of releases up to 0.33 go first (ADR-0054), then the coldest workspace dirs are
//!    evicted (their blobs orphan and the next sweep reclaims them), and a group goes with the last
//!    of its workspaces that still held a view. This is
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

use crate::data_root::DataRoot;
use crate::store::{WORKSPACE_MARKER_FILE, acquire_lock, read_workspace_marker};
use crate::store_gc::{GcError, GcReport, dir_size, gc_global_blobs_in, read_dir};
use crate::store_gc_namespaces::Holds;
use crate::store_gc_protect::{ProtectedWorkspace, ProtectionReason, cache_grace};

/// Environment variable holding the cache byte budget in MiB. Unset ⇒ the user settings'
/// `[cache] budget_mb` ([`crate::config::user_settings`]), else the disk-aware default
/// ([`disk_aware_budget`]); `0` ⇒ unlimited (budget enforcement disabled).
pub const CACHE_BUDGET_ENV: &str = "BASEMIND_CACHE_BUDGET_MB";

/// Ceiling of the default cache budget: 20 GiB. Generous enough that a handful of monorepos never
/// trip it; small enough that an unbounded-growth regression is contained long before it threatens
/// a large disk.
const DEFAULT_CACHE_BUDGET_MIB: u64 = 20 * 1024;

/// Below the ceiling, the default budget is the cache's bytes plus the volume's free space divided
/// by this: the cache may take half of the space it could occupy.
const DISK_SHARE_DIVISOR: u64 = 2;

const MIB: u64 = 1024 * 1024;

/// How the cache byte budget is set, when enforcement is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheBudget {
    /// [`CACHE_BUDGET_ENV`] or the user settings name the budget, in bytes here; it is enforced
    /// as given.
    Fixed(u64),
    /// Neither names it: [`disk_aware_budget`], resolved at every sweep.
    DiskAware,
}

impl CacheBudget {
    /// The bytes a cache of `cache_bytes` is held to when its volume has `free_bytes` free
    /// (`None`: the free space could not be read).
    pub fn resolve(self, cache_bytes: u64, free_bytes: Option<u64>) -> u64 {
        match self {
            Self::Fixed(bytes) => bytes,
            Self::DiskAware => disk_aware_budget(cache_bytes, free_bytes),
        }
    }
}

/// The default budget: half of what the cache could occupy (its own bytes plus the volume's free
/// space), capped at `DEFAULT_CACHE_BUDGET_MIB`. The ceiling alone would not evict before a disk
/// with less than that free is full.
///
/// The share is of the sum, not of the free space alone: a budget of `cache + free / 2` re-derived
/// at every sweep grants a cache that grew into it half of the remaining free space again, and so on
/// until the disk is full. Half of the sum stops the cache where it is as large as the free space
/// left. An unreadable free space falls back to the ceiling.
pub fn disk_aware_budget(cache_bytes: u64, free_bytes: Option<u64>) -> u64 {
    let ceiling = DEFAULT_CACHE_BUDGET_MIB * MIB;
    free_bytes.map_or(ceiling, |free| {
        ceiling.min(cache_bytes.saturating_add(free) / DISK_SHARE_DIVISOR)
    })
}

/// Why `budget` cannot hold the cache back: a set budget larger than the cache's `cache_bytes`
/// plus the `free_bytes` left on its volume, which the disk fills before eviction starts. `None` for
/// a budget the disk can hold, the disk-aware default (half of that at most), disabled enforcement,
/// or free space that cannot be read.
pub fn budget_disk_warning(budget: Option<CacheBudget>, cache_bytes: u64, free_bytes: Option<u64>) -> Option<String> {
    let Some(CacheBudget::Fixed(budget_bytes)) = budget else {
        return None;
    };
    let free = free_bytes?;
    let capacity = cache_bytes.saturating_add(free);
    (budget_bytes > capacity).then(|| {
        format!(
            "the cache budget of {} MiB exceeds the {} MiB the disk can hold (the cache's {} MiB plus {} MiB \
             free), so the disk fills before eviction starts; lower {CACHE_BUDGET_ENV} or `[cache] budget_mb` \
             in {}, or unset both for the disk-aware default ({} MiB now)",
            budget_bytes / MIB,
            capacity / MIB,
            cache_bytes / MIB,
            free / MIB,
            crate::config::user_settings::USER_SETTINGS_FILE,
            disk_aware_budget(cache_bytes, Some(free)) / MIB,
        )
    })
}

/// [`budget_disk_warning`] for the machine-global cache, measured as a budget pass measures it.
/// Only a set budget is measured: no other can exceed the disk.
pub fn cache_budget_warning(data: &DataRoot, budget: Option<CacheBudget>) -> Result<Option<String>, GcError> {
    cache_budget_warning_at(&data.workspaces_dir(), &data.global_blobs_root(), budget, || {
        free_bytes_at(&data.cache_dir())
    })
}

/// [`cache_budget_warning`] against explicit directories, `free_bytes` reading the volume's free
/// space.
pub(crate) fn cache_budget_warning_at(
    workspaces_dir: &Path,
    blobs_root: &Path,
    budget: Option<CacheBudget>,
    free_bytes: impl FnOnce() -> Option<u64>,
) -> Result<Option<String>, GcError> {
    if !matches!(budget, Some(CacheBudget::Fixed(_))) {
        return Ok(None);
    }
    // The footprint a fixed budget is compared with: a newer basemind's workspaces and blob
    // namespaces are on the disk but outside what eviction can free. ~keep
    let newer = workspaces_of_a_newer_basemind(workspaces_dir);
    let cache_bytes = cache_footprint(workspaces_dir, blobs_root, crate::extract::SCHEMA_VER, &newer)?;
    Ok(budget_disk_warning(budget, cache_bytes, free_bytes()))
}

/// Free bytes on the volume holding `dir`, or its closest existing ancestor on a fresh machine;
/// `None` when the platform cannot say.
fn free_bytes_at(dir: &Path) -> Option<u64> {
    let existing = dir.ancestors().find(|path| path.exists()).unwrap_or(dir);
    fs2::available_space(existing).ok()
}

/// Workspaces touched more recently than this are considered only after every cold candidate.
/// Evicting a hot workspace risks an immediate rescan, but an absolute veto would let the cache
/// remain unbounded when hot workspaces alone exceed the configured budget.
const BUDGET_EVICTION_HOT_FLOOR: Duration = Duration::from_secs(24 * 60 * 60);

/// How much older than the probe a blob must be before the sweep after an eviction takes it for
/// one the eviction orphaned: file systems that keep whole-second mtimes round a blob written just
/// after the probe to just before it.
const PROBE_MTIME_SLACK: Duration = Duration::from_secs(2);

/// The slack the budget tests sweep with, whose blobs are all written within a second.
#[cfg(test)]
const TEST_PROBE_MTIME_SLACK: Duration = Duration::ZERO;

/// The grace the sweep after an eviction gives unreferenced blobs. Budget eviction runs holding
/// the daemon's blob-GC gate, so blobs the eviction orphaned can go at once without racing a scan
/// the daemon runs. A scan in another process is outside that gate, and its fresh blobs are in no
/// file map yet:
///
/// - while another process holds a workspace's writer lock, the ordinary grace period applies;
/// - otherwise only blobs older than `probed`, the instant before the locks were looked at, go.
///   Whatever a scan that started since has written is younger than that.
fn orphan_grace(workspaces_dir: &Path, probed: SystemTime) -> Duration {
    #[cfg(not(test))]
    let slack = PROBE_MTIME_SLACK;
    #[cfg(test)]
    let slack = TEST_PROBE_MTIME_SLACK;
    orphan_grace_with(workspaces_dir, probed, slack)
}

/// [`orphan_grace`] with `slack` for [`PROBE_MTIME_SLACK`].
fn orphan_grace_with(workspaces_dir: &Path, probed: SystemTime, slack: Duration) -> Duration {
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
        probed.elapsed().unwrap_or(Duration::ZERO) + slack
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
    /// Bytes reclaimed by those evictions, the evicted groups' included.
    #[serde(default)]
    pub evicted_bytes_freed: u64,
    /// Group directories evicted with the last of their workspaces that held a view.
    #[serde(default)]
    pub groups_evicted: usize,
    /// Cache budget applied by the most recent completed sweep.
    #[serde(default)]
    pub cache_budget_bytes: Option<u64>,
    /// Measured cache footprint after the most recent completed budget pass. Over budget it leaves
    /// out a newer basemind's workspaces; within budget it may count them (see
    /// [`EvictReport::total_bytes_before`]).
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
    /// The budget the pass held the cache to, resolved from the footprint it measured first.
    pub budget_bytes: u64,
    /// Combined cache footprint (workspaces + groups + blobs) measured before eviction, without what a newer
    /// basemind wrote. A pass that finds the cache within budget does not tell a newer basemind's
    /// workspaces apart (that reads a view of every workspace) and counts them here.
    pub total_bytes_before: u64,
    /// Combined cache footprint measured after the final eviction/reclamation step, counted as
    /// [`Self::total_bytes_before`] is.
    pub total_bytes_after: u64,
    /// Workspace dirs evicted.
    pub evicted: usize,
    /// Bytes those dirs occupied (stat'd before deletion).
    pub bytes_freed: u64,
    /// Per-view index databases of releases up to 0.33 removed before any workspace was evicted.
    pub legacy_indexes_evicted: usize,
    /// Bytes those databases occupied. Disjoint from [`Self::bytes_freed`].
    pub legacy_index_bytes_freed: u64,
    /// Group directories evicted because no workspace that holds a view belongs to them any more.
    pub groups_evicted: usize,
    /// Bytes those group directories occupied. Disjoint from [`Self::bytes_freed`].
    pub group_bytes_freed: u64,
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

/// What, besides the budget, decides what a budget pass may evict.
pub struct EvictionRules<'a> {
    /// Workspaces used more recently than this are evicted only after every colder one.
    pub(crate) hot_floor: Duration,
    /// A workspace with a view built more recently than this is never evicted.
    pub(crate) grace: Duration,
    /// What live basemind processes read. While one of a release up to 0.33, or one whose
    /// namespace cannot be told, runs, no per-view index is removed ahead of the workspaces.
    pub(crate) holds: &'a Holds,
}

struct EvictionCandidate {
    activity: SystemTime,
    hot: bool,
    has_views: bool,
    protected: Option<ProtectionReason>,
    dir: PathBuf,
}

/// The configured cache budget, as [`cache_budget_with`] resolved it when the engine configuration
/// was installed.
pub fn cache_budget() -> Option<CacheBudget> {
    crate::config::engine::with(|config| config.gc.budget)
}

/// A [`CACHE_BUDGET_ENV`] value in MiB with no user settings, read as [`cache_budget_with`] documents.
pub fn cache_budget_from(raw: Option<&str>) -> Option<CacheBudget> {
    cache_budget_with(raw, None)
}

/// The cache budget from [`CACHE_BUDGET_ENV`]'s value `raw` in MiB, else the user settings'
/// `[cache] budget_mb` (`settings_mib`), else [`CacheBudget::DiskAware`]. `0` from either, or an
/// unparseable `raw` (logged), disables enforcement and returns `None`.
pub fn cache_budget_with(raw: Option<&str>, settings_mib: Option<u64>) -> Option<CacheBudget> {
    let Some(raw) = raw else {
        return settings_mib.map_or(Some(CacheBudget::DiskAware), fixed_mib);
    };
    match raw.trim().parse::<u64>() {
        Ok(mib) => fixed_mib(mib),
        Err(_) => {
            crate::config::trust::warn_once(format!(
                "{CACHE_BUDGET_ENV}={raw:?} is not a number; disabling cache budget enforcement"
            ));
            None
        }
    }
}

/// A budget of `mib` MiB; `None` (no enforcement) for `0`.
fn fixed_mib(mib: u64) -> Option<CacheBudget> {
    (mib > 0).then(|| CacheBudget::Fixed(mib.saturating_mul(MIB)))
}

/// Path of the persisted last-GC state in the machine-global cache root.
pub fn gc_state_path(data: &DataRoot) -> PathBuf {
    data.cache_dir().join(GC_STATE_FILE)
}

/// Record what a destructive sweep did (best-effort: persistence failure is logged, never
/// propagated — losing a telemetry write must not fail the sweep that just succeeded).
pub fn persist_gc_state(data: &DataRoot, report: &GcReport) {
    persist_gc_state_at(&gc_state_path(data), report);
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
        groups_evicted: report.groups_evicted,
        cache_budget_bytes: report.cache_budget_bytes,
        cache_bytes_after: report.cache_bytes_after,
        hot_workspaces_evicted: report.hot_workspaces_evicted,
        locked_workspaces_skipped: report.locked_workspaces_skipped,
        protected_workspaces: report.protected_workspaces.clone(),
    };
    write_gc_state(path, &state);
}

/// Persist a failed or starved GC attempt while preserving the last completed sweep counters.
pub fn persist_gc_error(data: &DataRoot, error: &GcError) {
    persist_gc_error_at(&gc_state_path(data), error);
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
pub fn read_gc_state(data: &DataRoot) -> Option<GcState> {
    read_gc_state_at(&gc_state_path(data))
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

/// Enforce the cache budget against the machine-global cache: when workspaces (their git caches
/// included) + groups + blobs exceed `budget` resolved against the measured footprint and the
/// cache volume's free space, remove the per-view indexes of releases up to 0.33, then evict the
/// coldest workspace dirs, then hot dirs only if necessary, and the group directory of each evicted
/// workspace once no workspace holding a view uses it. Locked workspaces remain protected. Each
/// eviction immediately reclaims blobs that lost their final reference and remeasures the real
/// footprint before selecting another victim.
pub fn enforce_cache_budget(data: &DataRoot, budget: CacheBudget) -> Result<EvictReport, GcError> {
    let cache_dir = data.cache_dir();
    enforce_cache_budget_at(
        &data.workspaces_dir(),
        &data.global_blobs_root(),
        &data.global_blobs_dir(),
        crate::extract::SCHEMA_VER,
        // Free space is read after the footprint is measured, so a scan between the two cannot skew them. ~keep
        |cache_bytes| budget.resolve(cache_bytes, free_bytes_at(&cache_dir)),
        &EvictionRules {
            hot_floor: BUDGET_EVICTION_HOT_FLOOR,
            grace: cache_grace(),
            holds: &Holds::under(data),
        },
    )
}

/// [`enforce_cache_budget`] against a per-fixture cache whose blob store is one flat directory, with
/// no live basemind.
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
        |_| budget_bytes,
        &EvictionRules {
            hot_floor,
            grace,
            holds: &Holds::default(),
        },
    )
}

/// [`enforce_cache_budget`] against explicit directories. The footprint counts what this binary
/// can free: the workspaces except those holding a newer basemind's view, and everything under
/// `blobs_root` except the namespaces of a blob schema newer than `schema` (see
/// [`cache_footprint`]). Blobs an eviction orphans are reclaimed
/// in `blobs_dir`, the namespace the evicted workspaces of this version referenced. `budget` maps
/// the bytes the cache occupies ([`occupied_bytes`]), a newer basemind's workspaces and blob
/// namespaces included since they occupy the same disk, to the budget of the pass.
pub(crate) fn enforce_cache_budget_at(
    workspaces_dir: &Path,
    blobs_root: &Path,
    blobs_dir: &Path,
    schema: u16,
    budget: impl FnOnce(u64) -> u64,
    rules: &EvictionRules<'_>,
) -> Result<EvictReport, GcError> {
    // Within budget counting a newer basemind's workspaces, the cache is within it without them:
    // telling them apart reads a view of every workspace, so only a cache over budget pays for it.
    let occupied = occupied_bytes(workspaces_dir, blobs_root, &[])?;
    let budget_bytes = budget(occupied);
    let raw = occupied.saturating_sub(newer_namespace_bytes(blobs_root, schema)?);
    if raw <= budget_bytes {
        return Ok(EvictReport {
            budget_bytes,
            total_bytes_before: raw,
            total_bytes_after: raw,
            ..EvictReport::default()
        });
    }
    let mut newer = workspaces_of_a_newer_basemind(workspaces_dir);
    let total = if newer.is_empty() {
        raw
    } else {
        cache_footprint(workspaces_dir, blobs_root, schema, &newer)?
    };
    let mut report = EvictReport {
        budget_bytes,
        total_bytes_before: total,
        total_bytes_after: total,
        ..EvictReport::default()
    };
    if report.total_bytes_before <= budget_bytes {
        return Ok(report);
    }
    // This release reads no per-view index: removing them first costs no workspace its index. ~keep
    let legacy = crate::store_gc_legacy_views::evict_legacy_indexes_in(workspaces_dir, rules.holds, &newer)?;
    if legacy.reaped > 0 {
        report.legacy_indexes_evicted = legacy.reaped;
        report.legacy_index_bytes_freed = legacy.bytes_freed;
        report.total_bytes_after = cache_footprint(workspaces_dir, blobs_root, schema, &newer)?;
    }

    for candidate in eviction_candidates(workspaces_dir, &newer, rules.hot_floor, rules.grace)? {
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
                // A newer basemind wrote a view into it during the pass: its bytes are no longer
                // this binary's to count either.
                if reason == ProtectionReason::NewerVersion {
                    newer.push(candidate.dir.clone());
                    report.total_bytes_after = cache_footprint(workspaces_dir, blobs_root, schema, &newer)?;
                }
                continue;
            }
        };
        let group_bytes = match crate::store_layout::group_of_workspace(&candidate.dir) {
            Some(group) => crate::store_gc_groups::evict_idle_group(workspaces_dir, &group)?,
            None => 0,
        };
        if group_bytes > 0 {
            report.groups_evicted += 1;
            report.group_bytes_freed += group_bytes;
        }
        let probed = SystemTime::now();
        let reclaimed = gc_global_blobs_in(workspaces_dir, blobs_dir, orphan_grace(workspaces_dir, probed))?;
        report.blobs_removed += reclaimed.removed;
        report.blob_bytes_freed += reclaimed.bytes_freed;
        report.total_bytes_after = cache_footprint(workspaces_dir, blobs_root, schema, &newer)?;
        let measured_freed = bytes_before_candidate.saturating_sub(report.total_bytes_after);
        if size > 0 || group_bytes > 0 || reclaimed.bytes_freed > 0 {
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
    newer: &[PathBuf],
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
        let protected = if newer.contains(&dir) {
            Some(ProtectionReason::NewerVersion)
        } else {
            (has_views && idle < grace).then_some(ProtectionReason::RecentlyBuilt)
        };
        candidates.push(EvictionCandidate {
            activity,
            hot: idle < hot_floor,
            has_views,
            protected,
            dir,
        });
    }
    rank_candidates(&mut candidates, newer);
    Ok(candidates)
}

/// Protect the most recently used workspace and order `candidates` for eviction, cold before hot,
/// least recently used first. A tie on last activity is broken by the workspace path, so the
/// outcome does not depend on the order the filesystem listed the workspaces in: the greatest path
/// counts as the most recently used and is evicted last.
fn rank_candidates(candidates: &mut [EvictionCandidate], newer: &[PathBuf]) {
    // The most recently used workspace across every worktree of every repo is always protected,
    // however old it is and whatever the grace window says.
    if let Some(mru) = candidates
        .iter()
        .filter(|candidate| candidate.has_views && !newer.contains(&candidate.dir))
        .max_by(|a, b| (a.activity, &a.dir).cmp(&(b.activity, &b.dir)))
        .map(|candidate| candidate.dir.clone())
    {
        for candidate in candidates.iter_mut().filter(|candidate| candidate.dir == mru) {
            candidate.protected = Some(ProtectionReason::MostRecentlyUsed);
        }
    }
    candidates.sort_by(|a, b| (a.hot, a.activity, &a.dir).cmp(&(b.hot, b.activity, &b.dir)));
}

/// What became of a workspace picked for eviction.
enum Eviction {
    /// Its rebuildable state is gone; the bytes that freed.
    Done(u64),
    /// It was left whole.
    Refused(ProtectionReason),
}

fn evict_workspace(dir: &Path) -> Result<Eviction, GcError> {
    #[cfg(test)]
    tests::before_evict(dir);
    let Ok(lock) = acquire_lock(dir) else {
        tracing::debug!(workspace = %dir.display(), "over-budget workspace is locked; skipping");
        return Ok(Eviction::Refused(ProtectionReason::Locked));
    };
    if holds_a_newer_view(dir) {
        tracing::debug!(workspace = %dir.display(), "over-budget workspace was written by a newer basemind; skipping");
        return Ok(Eviction::Refused(ProtectionReason::NewerVersion));
    }
    if !crate::store_gc_live::rescue_legacy_memory(dir) {
        return Ok(Eviction::Refused(ProtectionReason::LegacyMemory));
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
pub(crate) fn holds_a_newer_view(workspace: &Path) -> bool {
    let views = workspace.join(crate::store::VIEWS_DIR);
    std::fs::read_dir(views).is_ok_and(|entries| entries.flatten().any(|view| view_is_newer(&view.path())))
}

/// Bytes of a file map that hold its header and first field.
const FILE_MAP_HEAD_BYTES: u64 = 32;

/// Whether the file map of `view_dir` was written by a newer basemind. Its first field is
/// `schema_ver`, so an older or newer schema is told from the first bytes of a map that can run to
/// many megabytes; a map of this schema, or a head of another shape, is judged by its version fields
/// ([`crate::store::written_by_newer`]), since a newer blob layout shares the schema. A map whose
/// version cannot be read counts as newer: it may be, and it is never this version's to evict.
fn view_is_newer(view_dir: &Path) -> bool {
    use std::io::Read as _;
    #[cfg(test)]
    tests::VIEW_READS.with(|reads| reads.set(reads.get() + 1));
    let mut head = Vec::new();
    let read = std::fs::File::open(view_dir.join(crate::store::INDEX_FILE))
        .and_then(|file| file.take(FILE_MAP_HEAD_BYTES).read_to_end(&mut head));
    if read.is_err() {
        return false;
    }
    match leading_schema_ver(&head).map(|found| found.cmp(&crate::extract::SCHEMA_VER)) {
        Some(std::cmp::Ordering::Greater) => true,
        Some(std::cmp::Ordering::Less) => false,
        _ => crate::store::written_by_newer(view_dir),
    }
}

/// The value of a msgpack map's first entry when its key is `schema_ver` and the value an
/// unsigned integer that fits a `u16`; `None` for any other head.
fn leading_schema_ver(head: &[u8]) -> Option<u16> {
    let entries = match *head.first()? {
        0x80..=0x8f => head.get(1..)?,
        0xde => head.get(3..)?,
        0xdf => head.get(5..)?,
        _ => return None,
    };
    let (len, key_and_rest) = match *entries.first()? {
        marker @ 0xa0..=0xbf => (usize::from(marker & 0x1f), entries.get(1..)?),
        0xd9 => (usize::from(*entries.get(1)?), entries.get(2..)?),
        _ => return None,
    };
    if key_and_rest.get(..len)? != b"schema_ver" {
        return None;
    }
    let value = key_and_rest.get(len..)?;
    match *value.first()? {
        small @ 0x00..=0x7f => Some(u16::from(small)),
        0xcc => value.get(1).copied().map(u16::from),
        0xcd => Some(u16::from_be_bytes([*value.get(1)?, *value.get(2)?])),
        _ => None,
    }
}

/// Derived state that can be rebuilt from source. Stable identity, the workspace marker and the
/// per-repo-group `repos/<group>/` (the memory notes and their vector stores, a sibling of
/// `workspaces/`, so never under `workspace`) are intentionally absent: budget enforcement must
/// never rotate an agent's identity or delete user-authored memory.
fn rebuildable_workspace_paths(workspace: &Path) -> [PathBuf; 8] {
    let lance = workspace.join(LANCE_WORKSPACE_DIR);
    [
        workspace.join(crate::store::VIEWS_DIR),
        workspace.join(crate::git_cache::GIT_CACHE_DIR),
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

/// The workspace dirs holding a view a newer basemind wrote, which [`evict_workspace`] refuses.
/// Decided once per pass, and only by a pass over budget: each check reads every view's file map.
fn workspaces_of_a_newer_basemind(workspaces_dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(workspaces_dir).map_or_else(
        |_| Vec::new(),
        |entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|dir| dir.is_dir() && holds_a_newer_view(dir))
                .collect()
        },
    )
}

/// The bytes the budget is held against: [`occupied_bytes`] without the blob namespaces of a
/// schema newer than `schema`. What is left out belongs to a newer basemind sharing the cache. This
/// binary never deletes it, so counting it would have it evict workspaces every cycle to make up
/// for bytes no eviction can free.
fn cache_footprint(workspaces_dir: &Path, blobs_root: &Path, schema: u16, newer: &[PathBuf]) -> Result<u64, GcError> {
    Ok(occupied_bytes(workspaces_dir, blobs_root, newer)?.saturating_sub(newer_namespace_bytes(blobs_root, schema)?))
}

/// The bytes the cache occupies on disk: the workspaces other than `newer`, the group directories
/// (`cache/groups/`, beside `workspaces_dir`) other than those of `newer`, and the whole blob store.
fn occupied_bytes(workspaces_dir: &Path, blobs_root: &Path, newer: &[PathBuf]) -> Result<u64, GcError> {
    let groups_dir = workspaces_dir
        .parent()
        .map(|cache| cache.join(crate::store_layout::GROUPS_DIR));
    let groups_bytes = match &groups_dir {
        Some(dir) => dir_size_or_zero(dir)?,
        None => 0,
    };
    let mut total = dir_size_or_zero(workspaces_dir)? + groups_bytes + dir_size_or_zero(blobs_root)?;
    for workspace in newer {
        total = total.saturating_sub(dir_size_or_zero(workspace)?);
    }
    if let Some(cache) = workspaces_dir.parent() {
        for group in crate::store_gc_groups::group_dirs_of(cache, newer) {
            total = total.saturating_sub(dir_size_or_zero(&group)?);
        }
    }
    Ok(total)
}

/// The bytes under `blobs_root` in the namespaces of a blob schema newer than `schema`.
fn newer_namespace_bytes(blobs_root: &Path, schema: u16) -> Result<u64, GcError> {
    if !blobs_root.exists() {
        return Ok(0);
    }
    let mut total = 0;
    for entry in read_dir(blobs_root)?.flatten() {
        let newer = entry
            .file_name()
            .to_str()
            .and_then(crate::store_gc_namespaces::namespace_schema)
            .is_some_and(|found| found > schema);
        if newer && entry.path().is_dir() {
            total += dir_size(&entry.path())?;
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
#[path = "store_gc_budget_group_tests.rs"]
mod group_tests;

#[cfg(test)]
#[path = "store_gc_protect_tests.rs"]
mod protect_tests;
