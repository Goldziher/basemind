//! The git-history index of a daemon-hosted session: a [`GitHistoryIndex`] whose reads run against
//! the database the daemon holds, in the daemon's own process.
//!
//! Fjall takes an exclusive lock on the database directory, even for a read-only open, so
//! `git-history.fjall/` has one holder: the daemon, which builds it. A session the daemon hosts reads
//! it through [`HistoryHost`], in process. No other process reads it while the daemon is up: a CLI
//! command forwards its whole call to the daemon, and one the daemon does not host (another view)
//! live-walks.
//!
//! ## Blocking bridge
//!
//! The history tools call the index from synchronous code, so a hosted read has to block. It does so
//! via [`tokio::task::block_in_place`], which requires the multi-threaded runtime the daemon runs.
//! Off a multi-thread runtime (a `current_thread` test, a rayon worker) the call degrades to `None`
//! rather than panicking, and the caller live-walks.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use tokio::runtime::{Handle, RuntimeFlavor};

use super::proto::{GitHistoryOp, GitHistoryReply};
use crate::git::CommitInfo;
use crate::git_history::{GitHistoryError, GitHistoryIndex, HistoryForwarder};

/// In-process access to the daemon-held git-history index, for a daemon-hosted connection.
///
/// A daemon that hosts the rmcp router ([`crate::mcp::BasemindServer::from_shared`]) runs the same
/// tool bodies as a `daemon_writer` serve, whose history reads FORWARD to the daemon — but here the
/// daemon would be dialing itself. This port lets those reads run in-process instead. The implementor
/// (the [`Broker`](crate::comms::daemon::Broker)) owns the repo's single open handle and its build
/// lock, so a hosted caller and a socket-forwarding one share exactly one index and one build.
///
/// The method is async so [`RemoteHistory::call`]'s existing `block_in_place`/`block_on` bridge drives
/// it with no nested runtime; it is boxed so the trait stays object-safe without an `async-trait` dep.
pub trait HistoryHost: Send + Sync {
    /// Run one op against the daemon's in-process index.
    fn run_history(
        &self,
        root: PathBuf,
        op: GitHistoryOp,
    ) -> Pin<Box<dyn Future<Output = Result<GitHistoryReply, GitHistoryError>> + Send + '_>>;
}

/// A git-history index whose storage lives in the daemon, read in the daemon's own process.
#[derive(Clone)]
pub struct RemoteHistory {
    /// Canonical workspace root — selects the repo's index daemon-side.
    root: PathBuf,
    /// The daemon itself, which holds the index.
    host: Arc<dyn HistoryHost>,
}

impl RemoteHistory {
    /// A daemon-backed handle whose daemon is *this process*: reads run in-process through `host`
    /// rather than over the socket. Used by a daemon-hosted connection.
    pub fn hosted(root: PathBuf, host: Arc<dyn HistoryHost>) -> Self {
        Self { root, host }
    }

    /// The in-process host. Drives the startup sync in-process.
    pub(crate) fn host(&self) -> Option<Arc<dyn HistoryHost>> {
        Some(Arc::clone(&self.host))
    }

    /// Block the current task on one forwarded op. See the module docs for the runtime contract.
    fn call(&self, op: GitHistoryOp) -> Option<GitHistoryReply> {
        let handle = Handle::try_current().ok()?;
        if handle.runtime_flavor() != RuntimeFlavor::MultiThread {
            tracing::debug!("git-history: no multi-thread runtime to forward on; falling back to the live walk");
            return None;
        }
        tokio::task::block_in_place(|| handle.block_on(self.call_async(op)))
    }

    async fn call_async(&self, op: GitHistoryOp) -> Option<GitHistoryReply> {
        self.host
            .run_history(self.root.clone(), op)
            .await
            .inspect_err(|error| tracing::warn!(%error, "git-history: in-process query failed; tools live-walk"))
            .ok()
    }
}

impl HistoryForwarder for RemoteHistory {
    /// The HEAD the daemon's index is synced to, or `None` when unbuilt / unreachable. `None` makes
    /// the caller's freshness check fail closed — it live-walks rather than trusting an index it
    /// could not confirm.
    fn indexed_head(&self) -> Option<String> {
        match self.call(GitHistoryOp::IndexedHead)? {
            GitHistoryReply::IndexedHead(head) => head,
            _ => None,
        }
    }

    /// Run a commit-returning op against the daemon's index. An unreachable daemon or a shape
    /// mismatch yields an empty result; the freshness check in
    /// [`git_history_if_fresh`](crate::mcp) has already gated this call, so the only way to get here
    /// with a dead daemon is a race, and an empty page is the safe answer.
    fn commits(&self, op: GitHistoryOp) -> Vec<CommitInfo> {
        match self.call(op) {
            Some(GitHistoryReply::Commits(commits)) => commits,
            _ => Vec::new(),
        }
    }

    fn hosts_in_process(&self) -> bool {
        true
    }
}

/// A DAEMON-BACKED handle whose daemon is *this process* — a daemon-hosted connection. Instead of
/// dialing the daemon over its own socket (a loopback), history ops run in-process through `host`,
/// which shares the daemon's single open handle and per-repo build lock. Takes no lock and performs
/// no IO.
pub fn hosted_index(root: PathBuf, host: Arc<dyn HistoryHost>) -> GitHistoryIndex {
    GitHistoryIndex::forwarded(Arc::new(RemoteHistory::hosted(root, host)))
}

/// The in-process host of a daemon-hosted handle, if any. A hosted connection uses it to run the
/// startup git-history sync directly (through the daemon's build lock) instead of forwarding the
/// `Sync` op back over the socket. `None` for a local handle or a socket-forwarding one.
///
/// A forwarder that [hosts in process](HistoryForwarder::hosts_in_process) but is not a
/// [`RemoteHistory`] yields `None` too; that is a wiring bug, logged here, and the caller must still
/// not fall back to the socket (it checks `hosts_in_process` itself).
pub(crate) fn history_host(index: &GitHistoryIndex) -> Option<Arc<dyn HistoryHost>> {
    let forwarder = index.forwarder()?;
    let any: &dyn std::any::Any = forwarder.as_ref();
    let host = any.downcast_ref::<RemoteHistory>().and_then(RemoteHistory::host);
    if host.is_none() && forwarder.hosts_in_process() {
        tracing::error!("git-history: an in-process forwarder exposes no host; its startup sync is skipped");
    }
    host
}

/// Whether a machine daemon is up — and therefore holds this machine's git-history databases.
///
/// This is the ROUTING question for a process that does not already know the answer. `serve` knows it
/// by construction (it is the process that brings the daemon up, and carries `daemon_writer`); the
/// one-shot CLI does not, and must not guess. Guessing wrong is the whole failure: with a daemon up,
/// a local open can never win fjall's exclusive directory lock, so the CLI burns the retry ladder
/// (`GH_OPEN_RETRIES` × `GH_OPEN_BACKOFF`, ~1.3 s on every invocation, git or not) and then silently
/// live-walks — on the exact machine whose index is built, fresh, and hundreds of times faster.
///
/// Cheap by design, because it sits on the startup path of every CLI invocation:
///
/// * no endpoint on disk ⇒ no daemon, decided by one `stat` (Unix). This is the standalone case, and
///   it must stay free: [`probe_alive`](crate::comms::singleton::probe_alive) retries four times with
///   a 100 ms backoff before declaring a daemon dead — right for reclaiming a socket, a ~300 ms tax
///   here on a machine that simply has no daemon.
/// * otherwise one connect + ping. A live daemon answers on the first attempt; only an ORPHANED
///   socket (a crashed daemon) pays the full probe, and it is then correctly judged dead.
pub fn daemon_is_up() -> bool {
    let Ok(paths) = crate::comms::singleton::resolve_paths() else {
        return false;
    };
    #[cfg(unix)]
    if !paths.socket_path.exists() {
        return false;
    }
    // A socket file whose listener ACCEPTS a connection is a live daemon: an orphaned file (crashed
    // daemon) refuses. The ping round-trip of `probe_alive` is the wrong test here, because a busy
    // daemon (CPU-starved, mid-sync) can miss its 500 ms read timeout four times running; the CLI
    // then concludes "no daemon", loses the lock race for the index it could have proxied to, and
    // silently live-walks. Fall back to the full probe only when the connect itself fails.
    #[cfg(unix)]
    if std::os::unix::net::UnixStream::connect(&paths.socket_path).is_ok() {
        return true;
    }
    crate::comms::singleton::probe_alive(&paths.socket_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DecliningHost;

    impl HistoryHost for DecliningHost {
        fn run_history(
            &self,
            _root: PathBuf,
            _op: GitHistoryOp,
        ) -> Pin<Box<dyn Future<Output = Result<GitHistoryReply, GitHistoryError>> + Send + '_>> {
            Box::pin(async { Err(GitHistoryError::Disabled) })
        }
    }

    #[test]
    fn a_hosted_index_syncs_in_process_and_never_over_the_socket() {
        let index = hosted_index(PathBuf::from("/repo"), Arc::new(DecliningHost));
        let forwarder = index.forwarder().expect("a hosted index is daemon-backed");
        assert!(forwarder.hosts_in_process());
        assert!(history_host(&index).is_some());
    }
}
