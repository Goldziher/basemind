//! Param and response types for the cache admin MCP tools (`cache_stats`,
//! `cache_gc`, `cache_clear`).
//!
//! Split out of `types.rs` to keep that file under the 1000-line cap. The
//! response structs mirror the `store_gc` layer's `Serialize`-only structs and
//! add the `JsonSchema` derive the MCP surface requires.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CacheStatsParams {}

/// MCP-facing mirror of [`crate::store_gc::CacheStats`]. The store-layer struct
/// derives `Serialize` but not `JsonSchema`; this clone adds the schema derive the
/// MCP surface needs and converts via [`From`].
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct CacheStatsResponse {
    /// Recursive byte size of `blobs/`.
    pub blobs_bytes: u64,
    /// Recursive byte size of `views/`.
    pub views_bytes: u64,
    /// Recursive byte size of `lance/`.
    pub lance_bytes: u64,
    /// Recursive byte size of `git-cache/`.
    pub git_cache_bytes: u64,
    /// Byte size of `telemetry.jsonl`.
    pub telemetry_bytes: u64,
    /// Recursive byte size of the git-history index (`git-history.fjall/`).
    pub git_history_bytes: u64,
    /// Recursive byte size of the entire `.basemind/` tree — the ground-truth footprint (matches
    /// `du`). The component fields break this down; unattributed bytes are in `other_bytes`.
    pub total_bytes: u64,
    /// Bytes under `.basemind/` not attributed to a named component (`total_bytes` minus the
    /// component sum): legacy `index.msgpack`, lock/id/config sidecars, `.gitignore`, etc.
    pub other_bytes: u64,
    /// Total blob files on disk (every suffix counts as one file).
    pub blob_count: usize,
    /// Blob files whose hex stem is referenced by no view — reclaimable by `cache_gc`. Meaningful
    /// only when `blob_accounting_ok` is `true`.
    pub orphan_blob_count: usize,
    /// Whether orphan accounting ran. `false` = a view index was unreadable (stale schema /
    /// corruption), so `orphan_blob_count` is `0` because it was skipped, not because there are
    /// none; the size fields remain accurate. Re-scan to restore accounting.
    pub blob_accounting_ok: bool,
    /// Per-view indexed file count, `(view_name, file_count)`.
    pub per_view_file_count: Vec<(String, usize)>,
    /// Current resident set size (physical RAM) of the process serving this call, in bytes;
    /// `null` when unreadable. Inside `basemind serve` this is the live MCP server process.
    pub rss_bytes: Option<u64>,
    /// Peak resident set size of the serving process over its lifetime, in bytes; `null` when
    /// unreadable.
    pub peak_rss_bytes: Option<u64>,
    /// Outcome of the most recent destructive GC sweep, or `null` when none has ever completed
    /// on this machine (which, on a long-lived install, means GC is not running — investigate).
    pub last_gc: Option<LastGcResponse>,
}

/// MCP-facing mirror of [`crate::store_gc_budget::GcState`] — the persisted outcome of the most
/// recent destructive GC sweep.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct LastGcResponse {
    /// When the sweep finished (seconds since the Unix epoch).
    pub at_epoch_secs: u64,
    /// Blob files inspected.
    pub scanned: usize,
    /// Orphan blob files removed.
    pub removed: usize,
    /// Blob bytes reclaimed.
    pub bytes_freed: u64,
    /// Orphaned workspace dirs reaped.
    pub workspaces_reaped: usize,
    /// Bytes reclaimed by reaping those dirs.
    pub workspace_bytes_freed: u64,
    /// Cold workspace dirs evicted by cache-budget enforcement.
    pub workspaces_evicted: usize,
    /// Bytes reclaimed by those evictions.
    pub evicted_bytes_freed: u64,
}

impl From<crate::store_gc_budget::GcState> for LastGcResponse {
    fn from(s: crate::store_gc_budget::GcState) -> Self {
        Self {
            at_epoch_secs: s.at_epoch_secs,
            scanned: s.scanned,
            removed: s.removed,
            bytes_freed: s.bytes_freed,
            workspaces_reaped: s.workspaces_reaped,
            workspace_bytes_freed: s.workspace_bytes_freed,
            workspaces_evicted: s.workspaces_evicted,
            evicted_bytes_freed: s.evicted_bytes_freed,
        }
    }
}

impl From<crate::store_gc::CacheStats> for CacheStatsResponse {
    fn from(s: crate::store_gc::CacheStats) -> Self {
        Self {
            blobs_bytes: s.blobs_bytes,
            views_bytes: s.views_bytes,
            lance_bytes: s.lance_bytes,
            git_cache_bytes: s.git_cache_bytes,
            telemetry_bytes: s.telemetry_bytes,
            git_history_bytes: s.git_history_bytes,
            total_bytes: s.total_bytes,
            other_bytes: s.other_bytes,
            blob_count: s.blob_count,
            orphan_blob_count: s.orphan_blob_count,
            blob_accounting_ok: s.blob_accounting_ok,
            per_view_file_count: s.per_view_file_count,
            rss_bytes: s.rss_bytes,
            peak_rss_bytes: s.peak_rss_bytes,
            last_gc: s.last_gc.map(LastGcResponse::from),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CacheGcParams {}

/// MCP-facing mirror of [`crate::store_gc::GcReport`] — see [`CacheStatsResponse`]
/// for why the store struct's `JsonSchema` is re-derived here.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct CacheGcResponse {
    /// Total blob files inspected.
    pub scanned: usize,
    /// Orphan blob files removed.
    pub removed: usize,
    /// Bytes reclaimed by the removals.
    pub bytes_freed: u64,
}

impl From<crate::store_gc::GcReport> for CacheGcResponse {
    fn from(r: crate::store_gc::GcReport) -> Self {
        Self {
            scanned: r.scanned,
            removed: r.removed,
            bytes_freed: r.bytes_freed,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CacheClearParams {
    /// Component to clear: `blobs|views|lance|git-cache|telemetry|all`.
    pub component: String,
    /// Required gate for the destructive components (`blobs`, `views`) that back
    /// the live code map. Ignored for the non-live caches.
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct CacheClearResponse {
    /// Canonical token of the component that was targeted.
    pub component: String,
    /// True when the component was actually cleared.
    pub cleared: bool,
}
