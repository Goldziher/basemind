//! How many filesystem watches the working-tree watcher holds, and how many directories the OS
//! watch limit left unwatched.
//!
//! Linux inotify watches one directory per watch, and a user's watches across every process are
//! capped by the `fs.inotify.max_user_watches` sysctl. Many worktrees exhaust it: each refused
//! registration leaves a directory whose edits never reach the index. The watcher publishes its
//! counts here, `index_status` reports them, and the daemon log names the sysctl to raise.
//! macOS and Windows watch the tree with one recursive watch and publish nothing.

use std::sync::atomic::{AtomicUsize, Ordering};

use tracing::{info, warn};

/// The sysctl capping one user's inotify watches.
pub const MAX_USER_WATCHES_SYSCTL: &str = "fs.inotify.max_user_watches";

/// Where Linux exposes [`MAX_USER_WATCHES_SYSCTL`].
#[cfg(target_os = "linux")]
const MAX_USER_WATCHES_PATH: &str = "/proc/sys/fs/inotify/max_user_watches";

/// The watcher's watch counts, written by the watcher thread and read by `index_status`.
#[derive(Debug, Default)]
pub struct WatchHealth {
    held: AtomicUsize,
    unwatched: AtomicUsize,
}

impl WatchHealth {
    /// Publish the watcher's counts: `held` watches registered, `unwatched` directories the watch
    /// limit refused. Logs a warning naming the sysctl when more directories went unwatched than
    /// last published, and a note when every directory is watched again.
    pub fn record(&self, held: usize, unwatched: usize) {
        self.held.store(held, Ordering::Relaxed);
        let before = self.unwatched.swap(unwatched, Ordering::Relaxed);
        if unwatched > before {
            warn!(held, unwatched, "{}", exhaustion_note(unwatched, max_user_watches()));
        } else if unwatched == 0 && before > 0 {
            info!(held, "the inotify watch limit no longer leaves directories unwatched");
        }
    }

    /// Drop the counts once the watcher has exited: it holds no watches, and the directories it
    /// could not watch are no longer its to report. Silent, unlike [`record`](Self::record).
    pub fn clear(&self) {
        self.held.store(0, Ordering::Relaxed);
        self.unwatched.store(0, Ordering::Relaxed);
    }

    /// Watches the watcher holds.
    pub fn held(&self) -> usize {
        self.held.load(Ordering::Relaxed)
    }

    /// Directories the watch limit refused, which are not watched.
    pub fn unwatched(&self) -> usize {
        self.unwatched.load(Ordering::Relaxed)
    }
}

/// Is `error` the OS refusing a watch because the watch limit is reached? notify maps inotify's
/// `ENOSPC` to [`notify::ErrorKind::MaxFilesWatch`]; a raw `ENOSPC` is `StorageFull`.
pub fn is_watch_limit(error: &notify::Error) -> bool {
    match &error.kind {
        notify::ErrorKind::MaxFilesWatch => true,
        notify::ErrorKind::Io(io) => io.kind() == std::io::ErrorKind::StorageFull,
        _ => false,
    }
}

/// The current value of [`MAX_USER_WATCHES_SYSCTL`]; `None` off Linux or when it cannot be read.
pub fn max_user_watches() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(MAX_USER_WATCHES_PATH)
            .ok()
            .and_then(|raw| parse_limit(&raw))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The limit as `/proc` writes it: a decimal number and a newline.
#[cfg(any(target_os = "linux", test))]
fn parse_limit(raw: &str) -> Option<u64> {
    raw.trim().parse().ok()
}

/// What to tell the user when `unwatched` directories went unwatched under the watch `limit`.
pub fn exhaustion_note(unwatched: usize, limit: Option<u64>) -> String {
    let current = limit.map_or_else(|| "unknown".to_string(), |limit| limit.to_string());
    format!(
        "the inotify watch limit is reached: {unwatched} directories are not watched, so their \
         edits are not indexed until a rescan; raise {MAX_USER_WATCHES_SYSCTL} (currently \
         {current}), e.g. `sudo sysctl -w {MAX_USER_WATCHES_SYSCTL}=1048576`, then run \
         `basemind comms stop` so the next call starts a daemon that watches them"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_watch_is_recognised_as_the_watch_limit() {
        assert!(is_watch_limit(&notify::Error::new(notify::ErrorKind::MaxFilesWatch)));
        assert!(is_watch_limit(&notify::Error::io(std::io::Error::from(
            std::io::ErrorKind::StorageFull
        ))));
        assert!(!is_watch_limit(&notify::Error::path_not_found()));
        assert!(!is_watch_limit(&notify::Error::io(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        ))));
    }

    #[test]
    fn the_note_names_the_sysctl_and_the_current_limit() {
        assert_eq!(
            exhaustion_note(3, Some(8192)),
            "the inotify watch limit is reached: 3 directories are not watched, so their edits are \
             not indexed until a rescan; raise fs.inotify.max_user_watches (currently 8192), e.g. \
             `sudo sysctl -w fs.inotify.max_user_watches=1048576`, then run `basemind comms stop` \
             so the next call starts a daemon that watches them"
        );
        assert!(exhaustion_note(1, None).contains("fs.inotify.max_user_watches (currently unknown)"));
    }

    #[test]
    fn the_limit_parses_as_proc_writes_it() {
        assert_eq!(parse_limit("65536\n"), Some(65536));
        assert_eq!(parse_limit("garbage"), None);
    }

    #[test]
    fn recorded_counts_read_back() {
        let health = WatchHealth::default();
        assert_eq!((health.held(), health.unwatched()), (0, 0));
        health.record(10, 4);
        assert_eq!((health.held(), health.unwatched()), (10, 4));
        health.record(14, 0);
        assert_eq!((health.held(), health.unwatched()), (14, 0));
    }

    #[test]
    fn clearing_drops_the_counts() {
        let health = WatchHealth::default();
        health.record(10, 4);
        health.clear();
        assert_eq!((health.held(), health.unwatched()), (0, 0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_reads_the_limit_from_proc() {
        assert!(max_user_watches().is_some_and(|limit| limit > 0));
    }
}
