//! How a process opens the workspace it serves: the engine's [`OpenOptions`] with this process's
//! hooks, and the git-history handle it may hold.

use std::path::Path;
use std::sync::Arc;

use super::history_sync;
#[cfg(feature = "memory")]
use super::identity;
use crate::engine::workspace::OpenOptions;

/// The engine's open options for a host with the given switches, wired to this process's
/// git-history sync and agent identity.
pub(crate) fn open_options(background: bool, read_only: bool, lazy_cache: bool) -> OpenOptions {
    OpenOptions {
        background,
        read_only,
        forwards_writes: false,
        lazy_cache,
        history_sync: Some(history_sync::request),
        #[cfg(feature = "memory")]
        memory_owner: identity::resolve_agent_id,
    }
}

/// The git-history handle this process gets, if any. Fjall's directory lock is exclusive — even a
/// read-only open takes it — so `git-history.fjall/` has exactly one holder machine-wide, and the
/// only question here is whether that holder is us or the daemon:
///
/// * **a daemon is up**: the DAEMON holds the database. A session it hosts reads it in process
///   through the daemon's handle. Any other session (a CLI command on a view the daemon does not
///   host) gets no handle and live-walks: it cannot win the lock, and trying would burn the retry
///   ladder on every invocation. The CLI asks whether a daemon is up, which is one `stat` when
///   there is none (see [`crate::comms::singleton::daemon_is_up`]).
/// * **no daemon** (a standalone process, or a non-`comms` build): this process holds the
///   database and builds it in-process, as before. Nobody else can, and history tools would
///   otherwise permanently live-walk.
/// * **read-only fallback** (another process owns the write lock, no daemon): no handle — history
///   tools live-walk, visibly (`partial: true`). A one-shot CLI command beside such a process
///   does not try the database at all, and tries a free one once, never the retry ladder.
pub(crate) fn open_git_history(
    history_dir: &Path,
    has_repo: bool,
    background: bool,
    read_only: bool,
) -> Option<Arc<crate::git_history::GitHistoryIndex>> {
    if !has_repo || !crate::git_history::index_enabled() {
        return None;
    }
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if crate::comms::singleton::daemon_is_up() {
        // ~keep The daemon holds the database and fjall's lock is exclusive even to a reader
        // ~keep (fjall `locked_file.rs` `try_lock`), so a session it does not host live-walks.
        return None;
    }
    // ~keep A one-shot query never syncs, so an index it would open empty would stay empty.
    if read_only || (!background && !crate::git_history::GitHistoryIndex::exists(history_dir)) {
        return None;
    }
    // ~keep A one-shot query holds no writer lock, so a held index is another process's for as
    // ~keep long as it runs: it live-walks at once instead of climbing the retry ladder (~1.5 s).
    let opened = if background {
        crate::git_history::GitHistoryIndex::open(history_dir)
    } else if crate::store_lock::writer_lock_is_held(history_dir) {
        return None;
    } else {
        crate::git_history::GitHistoryIndex::open_once(history_dir)
    };
    match opened {
        Ok(index) => Some(Arc::new(index)),
        Err(error) => {
            tracing::warn!(?error, "git-history index unavailable; tools will live-walk");
            None
        }
    }
}
