//! The engine's request and response types. Their doc comments are what an agent reads: a params
//! struct's field docs are its tool's schema descriptions (ADR-0018), so they move byte for byte.

pub(crate) mod code;
pub(crate) mod files;
pub(crate) mod git;

use serde::Serialize;

use crate::engine::Lifecycle;

/// Lifecycle label attached to a read-tool response when the server is not fully [`Ready`]. Lets an
/// agent distinguish "index still loading — retry" from a genuine empty result, so a query issued during
/// startup warmup or a from-scratch index build is never misread as "no matches".
///
/// [`Ready`]: crate::engine::Lifecycle::Ready
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub(crate) struct LifecycleNotice {
    /// Machine-readable state tag: `"warming_up"`, `"building_index"`, `"rescanning"`, or `"index_rebuilding"`.
    pub state: &'static str,
    /// Human/agent-readable explanation of what the server is doing and what it means for this result.
    pub message: &'static str,
    /// `true` when the caller should retry shortly for complete results (warming / building), `false`
    /// when the current result is usable but may be a moment stale (rescanning).
    pub retry: bool,
}

impl LifecycleNotice {
    /// Build the notice for `state`, or `None` when [`Ready`](Lifecycle::Ready) (the common path,
    /// so no notice field is serialized on a healthy response).
    pub(crate) fn for_state(state: Lifecycle) -> Option<Self> {
        match state {
            Lifecycle::Ready => None,
            Lifecycle::WarmingUp => Some(Self {
                state: "warming_up",
                message: "Index is warming up (loading the code map into memory). Results may be \
                          incomplete — retry in a moment for the full set.",
                retry: true,
            }),
            Lifecycle::BuildingIndex => Some(Self {
                state: "building_index",
                message: "Index is building for the first time. Results are incomplete — call \
                          `index_status {}` until `indexing` is false, then retry.",
                retry: true,
            }),
            Lifecycle::Rescanning => Some(Self {
                state: "rescanning",
                message: "An incremental rescan is in progress after a file change. Results are usable \
                          but may be a moment stale.",
                retry: false,
            }),
        }
    }

    /// The notice for a read-only session whose projected call / implementation indexes hit their
    /// byte cap. Such a session has no Fjall index to query, so those projections ARE the answer
    /// source; once capped, reference-shaped results are truncated rather than merely stale.
    /// `retry: false` because rerunning changes nothing — the fix is a larger
    /// `[resources] max_map_cache_mb`, or a session that owns the index.
    pub(crate) fn projections_capped() -> Self {
        Self {
            state: "projections_capped",
            message: "No daemon was reachable, so this session answered from in-memory reference \
                      projections that hit the `[resources] max_map_cache_mb` budget; reference / \
                      implementation results are truncated. Start the daemon or raise the budget \
                      for complete results.",
            retry: false,
        }
    }

    /// [`index_rebuilding`](Self::index_rebuilding) when `status` records a degraded scan, else `None`
    /// (healthy, or no sidecar yet: the never-scanned case belongs to the building-index lifecycle).
    pub(crate) fn for_scan_status(status: Option<&crate::store::StatusSidecar>) -> Option<Self> {
        status
            .is_some_and(crate::store::StatusSidecar::is_degraded)
            .then(Self::index_rebuilding)
    }

    /// The notice attached when nothing has been scanned: every list is empty for that reason, not
    /// because the query matched nothing.
    pub(crate) fn empty_index() -> Self {
        Self {
            state: "empty_index",
            message: "The index is empty: nothing has been scanned for this workspace, so this result \
                      says nothing about the code. Run `index_rescan {}` (or `basemind scan` \
                      in a shell) and retry.",
            retry: true,
        }
    }

    /// The notice for a workspace whose last scan is partial or whose resolve pass has not
    /// committed, notably an index wiped by a schema revision bump and rebuilding. Reference-shaped
    /// reads (`references` / `callers` / `implementations`) answer empty or short until it finishes.
    pub(crate) fn index_rebuilding() -> Self {
        Self {
            state: "index_rebuilding",
            message: "index rebuilding — results incomplete. A full scan is rebuilding the symbol / \
                      reference index; an empty `references` / `callers` / `implementations` answer \
                      does not mean there are none. Call `index_status {}` until `degraded` clears, then \
                      retry.",
            retry: true,
        }
    }
}

/// `skip_serializing_if` for a count that is reported only when something was left out.
pub(crate) fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// `serde(default)` for a flag that is on unless the caller turns it off.
pub(crate) fn default_true() -> bool {
    true
}

/// `truncated_reason` of a scan that stopped at its cap before the end of the matches.
pub(crate) const SCAN_CAP: &str = "scan_cap";
