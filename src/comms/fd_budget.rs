//! The daemon's file-descriptor budget, raised toward it at start.
//!
//! Each Fjall database keeps a table of open segment files: 150 by default on macOS and 900 on
//! Linux (fjall `db_config.rs`). The daemon holds an index and a memory store for each hot
//! workspace, a git-history database per repository it serves, and the comms store, plus up to
//! [`MAX_CONNECTIONS`](super::accept_gate::MAX_CONNECTIONS) connections on each front-end. A soft
//! `RLIMIT_NOFILE` of 256 (the macOS default) runs out long before that; accepts then fail with
//! `EMFILE`.

/// Fjall's default open-file cache per database on this platform.
#[cfg(target_os = "macos")]
const FJALL_FILES_PER_DB: u64 = 150;
#[cfg(not(target_os = "macos"))]
const FJALL_FILES_PER_DB: u64 = 900;

/// Open-file cache of the comms store, which holds a few small keyspaces.
pub const COMMS_STORE_CACHED_FILES: usize = 32;

/// Git-history databases the budget assumes open at once.
const GIT_HISTORIES: u64 = 8;
/// Front-ends that accept connections: the socket or pipe, and HTTP.
const FRONT_ENDS: u64 = 2;
/// Descriptors for everything else: logs, lock files, watchers, the runtime, child pipes.
const SLACK: u64 = 512;

/// The descriptors the daemon may hold at its configured caps.
pub fn wanted() -> u64 {
    let hot = super::workspace_pool::DEFAULT_HOT_CAP as u64;
    let databases = hot * 2 + GIT_HISTORIES;
    databases * FJALL_FILES_PER_DB
        + COMMS_STORE_CACHED_FILES as u64
        + FRONT_ENDS * super::accept_gate::MAX_CONNECTIONS as u64
        + SLACK
}

/// The soft limit to set: `wanted`, never above the hard limit or the platform's per-process
/// ceiling, and never below what is already set.
#[cfg(unix)]
fn target_soft(soft: u64, hard: u64, wanted: u64, ceiling: u64) -> u64 {
    wanted.min(hard).min(ceiling).max(soft)
}

/// Raise the soft `RLIMIT_NOFILE` toward [`wanted`] and log the outcome. Returns the soft limit in
/// effect afterwards, `None` where it cannot be read.
#[cfg(unix)]
pub fn raise_to_budget() -> Option<u64> {
    let wanted = wanted();
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a live `rlimit` that `getrlimit` fills.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return None;
    }
    // macOS refuses a soft limit above `OPEN_MAX` even when the hard limit is unlimited. ~keep
    let ceiling = if cfg!(target_os = "macos") { 10_240 } else { u64::MAX };
    let soft = limit.rlim_cur;
    let target = target_soft(soft, limit.rlim_max, wanted, ceiling);
    if target > soft {
        let raised = libc::rlimit {
            rlim_cur: target,
            rlim_max: limit.rlim_max,
        };
        // SAFETY: `raised` is a live `rlimit` that `setrlimit` only reads.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
            limit.rlim_cur = raised.rlim_cur;
        }
    }
    let in_effect = limit.rlim_cur;
    if in_effect < wanted {
        tracing::warn!(
            soft = in_effect,
            hard = limit.rlim_max,
            wanted,
            "comms: the open-file limit is below the daemon's budget; accepts may fail with EMFILE"
        );
    } else {
        tracing::info!(soft = in_effect, wanted, "comms: open-file limit");
    }
    Some(in_effect)
}

/// Windows has no `RLIMIT_NOFILE`; its handle table is large.
#[cfg(not(unix))]
pub fn raise_to_budget() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn the_soft_limit_rises_to_the_budget_within_the_hard_limit_and_never_falls() {
        assert_eq!(target_soft(256, u64::MAX, 4000, 10_240), 4000);
        assert_eq!(target_soft(256, 1024, 4000, 10_240), 1024, "capped by the hard limit");
        assert_eq!(
            target_soft(256, u64::MAX, 40_000, 10_240),
            10_240,
            "capped by the ceiling"
        );
        assert_eq!(target_soft(65_536, u64::MAX, 4000, u64::MAX), 65_536, "never lowered");
    }

    #[test]
    fn the_budget_covers_every_database_cache_and_the_connection_caps() {
        let connections = 2 * super::super::accept_gate::MAX_CONNECTIONS as u64;
        assert!(wanted() > connections + 8 * FJALL_FILES_PER_DB);
    }

    #[cfg(unix)]
    #[test]
    fn raising_leaves_at_least_the_budget_or_the_hard_limit() {
        let soft = raise_to_budget().expect("readable");
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: as in `raise_to_budget`.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
        assert_eq!(limit.rlim_cur, soft);
        assert!(soft >= wanted().min(limit.rlim_max).min(10_240));
    }
}
