//! Which workspaces cache-budget eviction must never touch, even when the cache is over budget.
//!
//! The budget exists to bound idle, forgotten caches. A workspace the user is working in right now
//! is the opposite: evicting it deletes the index the next query needs, and when that single
//! workspace is larger than the budget (a 28 GB monorepo against the 20 GiB default) the budget
//! would otherwise delete it minutes after the scan that built it, every time. Protection is
//! therefore a veto, not a sort key: a protected workspace is skipped, reported, and the cache is
//! left over budget with a loud warning (`gc-state.json`, `cache_stats.last_gc`, `basemind doctor`).
//!
//! A workspace is protected when any of these hold:
//!
//! | Reason | Meaning |
//! |---|---|
//! | [`ProtectionReason::Locked`] | A live writer (`serve`, `watch`, the daemon, a scan) holds its `.lock`. |
//! | [`ProtectionReason::MostRecentlyUsed`] | Newest activity among workspaces that still hold a view. |
//! | [`ProtectionReason::RecentlyBuilt`] | Built or rescanned within the grace window ([`cache_grace`]). |
//! | [`ProtectionReason::NewerVersion`] | A view of it was written by a newer basemind sharing the cache. |
//! | [`ProtectionReason::LegacyMemory`] | A view's index holds pre-0.30 memory notes that could not be moved. |
//!
//! "Used" is the newest mtime of `workspace.json`, `status.json` and each view's `index.msgpack`;
//! every linked git worktree is its own workspace key, so the most recent one across all worktrees
//! of all repos wins, not one per repo. A workspace with no view left (already evicted) holds
//! nothing worth protecting and never counts as the most recent one.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Environment variable holding the post-build grace window in minutes. Unset gives
/// [`DEFAULT_CACHE_GRACE_MINUTES`]; `0` disables the window (the most-recently-used and locked
/// protections stay).
pub const CACHE_GRACE_ENV: &str = "BASEMIND_CACHE_GRACE_MINUTES";

/// Default grace window: 6 hours, longer than any single scan plus the work session that follows.
const DEFAULT_CACHE_GRACE_MINUTES: u64 = 6 * 60;

/// Why a workspace was exempted from budget eviction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProtectionReason {
    /// A live writer holds the workspace lock.
    Locked,
    /// The most recently used workspace on the machine.
    MostRecentlyUsed,
    /// Built or rescanned within the grace window.
    RecentlyBuilt,
    /// A view of it was written by a newer basemind sharing the cache: that version's live index.
    NewerVersion,
    /// A view's index holds memory notes a release before 0.30 kept there, which could not be moved
    /// to the group store.
    LegacyMemory,
}

/// A workspace the budget pass left alone although the cache is over budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProtectedWorkspace {
    /// Workspace cache key (directory name).
    pub key: String,
    /// Canonical worktree root from `workspace.json`, when readable.
    pub root: Option<String>,
    /// On-disk size of the workspace directory (its blobs are shared and not counted).
    pub bytes: u64,
    /// Why it was protected.
    pub reason: ProtectionReason,
}

/// The configured grace window: the engine configuration's
/// ([`crate::config::engine::GcPolicy::grace`]).
pub fn cache_grace() -> Duration {
    crate::config::engine::with(|config| config.gc.grace)
}

/// A [`CACHE_GRACE_ENV`] value in minutes, defaulting to 6 hours. An unparseable value is logged and
/// falls back to the default (a typo must not silently remove a safety net).
pub fn cache_grace_from(raw: Option<&str>) -> Duration {
    let minutes = match raw {
        Some(raw) => raw.trim().parse::<u64>().unwrap_or_else(|_| {
            crate::config::trust::warn_once(format!(
                "{CACHE_GRACE_ENV}={raw:?} is not a number; using the default grace window"
            ));
            DEFAULT_CACHE_GRACE_MINUTES
        }),
        None => DEFAULT_CACHE_GRACE_MINUTES,
    };
    Duration::from_secs(minutes * 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grace_parses_default_zero_and_garbage() {
        assert_eq!(cache_grace_from(None), Duration::from_secs(6 * 3600));
        assert_eq!(cache_grace_from(Some("0")), Duration::ZERO, "0 disables the window");
        assert_eq!(cache_grace_from(Some("30")), Duration::from_secs(30 * 60));
        assert_eq!(
            cache_grace_from(Some("soon")),
            Duration::from_secs(6 * 3600),
            "garbage keeps the default"
        );
    }
}
