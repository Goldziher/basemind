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

/// The configured grace window: [`CACHE_GRACE_ENV`] in minutes, defaulting to 6 hours. An
/// unparseable value is logged and falls back to the default (a typo must not silently remove a
/// safety net).
pub fn cache_grace() -> Duration {
    let minutes = match std::env::var(CACHE_GRACE_ENV) {
        Ok(raw) => raw.trim().parse::<u64>().unwrap_or_else(|_| {
            tracing::warn!(value = %raw, "{CACHE_GRACE_ENV} is not a number; using the default grace window");
            DEFAULT_CACHE_GRACE_MINUTES
        }),
        Err(_) => DEFAULT_CACHE_GRACE_MINUTES,
    };
    Duration::from_secs(minutes * 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grace_env_parses_default_zero_and_garbage() {
        // SAFETY: test-only env mutation; no other test in this binary reads this var
        unsafe { std::env::remove_var(CACHE_GRACE_ENV) };
        assert_eq!(cache_grace(), Duration::from_secs(6 * 3600));
        unsafe { std::env::set_var(CACHE_GRACE_ENV, "0") };
        assert_eq!(cache_grace(), Duration::ZERO, "0 disables the window");
        unsafe { std::env::set_var(CACHE_GRACE_ENV, "30") };
        assert_eq!(cache_grace(), Duration::from_secs(30 * 60));
        unsafe { std::env::set_var(CACHE_GRACE_ENV, "soon") };
        assert_eq!(
            cache_grace(),
            Duration::from_secs(6 * 3600),
            "garbage keeps the default"
        );
        unsafe { std::env::remove_var(CACHE_GRACE_ENV) };
    }
}
