//! The serve-side half of the forwarded git-history path: a [`GitHistoryIndex`] backed by the
//! daemon instead of by a local fjall database.
//!
//! ## Why serve cannot just open the index
//!
//! Fjall takes an exclusive advisory lock on the database directory — even a read-only open takes
//! it. `git-history.fjall/` is therefore a single-holder resource. Under the daemon-as-sole-writer
//! model the daemon must hold it (it is the process that builds it, and N concurrent serve sessions
//! cannot each hold a lock only one of them can win). So a `daemon_writer` serve holds NO handle and
//! forwards its history reads here, exactly as it already forwards scans
//! ([`CommsRequest::Rescan`](crate::comms::protocol::CommsRequest::Rescan)) and precise
//! resolved-reference reads ([`ResolvedRefs`](crate::comms::protocol::CommsRequest::ResolvedRefs)).
//!
//! ## Cost
//!
//! One UDS round trip per history tool call (plus one for the freshness check), against ~37 µs for a
//! local indexed lookup and ~1.6–2.5 ms for the live walk it replaces. The ops are coarse — a whole
//! result page per round trip — so the forwarded path stays O(1) IPC per tool call.
//!
//! ## Blocking bridge
//!
//! The MCP history tools call the index from synchronous code inside their async bodies, so the
//! forwarded call has to block. It does so via [`tokio::task::block_in_place`], which requires the
//! multi-threaded runtime `basemind serve` runs on. Off a multi-thread runtime (a `current_thread`
//! test, a rayon worker) the call degrades to `None` rather than panicking, and the caller live-walks.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::sync::{Mutex, OnceCell};

use super::proto::{GitHistoryOp, GitHistoryReply, SyncOutcome};
use crate::comms::client::CommsClient;
use crate::comms::ids::AgentId;
use crate::git::CommitInfo;

/// A git-history index whose storage lives in the daemon. Cloneable; the lazily-established
/// connection is shared between clones.
#[derive(Clone)]
pub struct RemoteHistory {
    /// Canonical workspace root — selects the repo's index daemon-side.
    root: PathBuf,
    /// This session's agent identity, replayed on the connection's `Hello`.
    agent: AgentId,
    /// The query connection, dialed on first use. Dedicated to git-history reads: sharing serve's
    /// main comms client would serialize a history query behind a multi-minute forwarded scan (which
    /// holds that client's lock for its whole duration).
    client: Arc<OnceCell<Arc<Mutex<CommsClient>>>>,
}

impl RemoteHistory {
    /// A daemon-backed handle for the repo at `root`, identifying as `agent`. Connects lazily: the
    /// constructor performs no IO, so a serve whose daemon is unreachable still starts (its history
    /// tools live-walk).
    pub fn new(root: PathBuf, agent: AgentId) -> Self {
        Self {
            root,
            agent,
            client: Arc::new(OnceCell::new()),
        }
    }

    /// The HEAD the daemon's index is synced to, or `None` when unbuilt / unreachable. `None` makes
    /// the caller's freshness check fail closed — it live-walks rather than trusting an index it
    /// could not confirm.
    pub fn indexed_head(&self) -> Option<String> {
        match self.call(GitHistoryOp::IndexedHead)? {
            GitHistoryReply::IndexedHead(head) => head,
            _ => None,
        }
    }

    /// Run a commit-returning op against the daemon's index. An unreachable daemon or a shape
    /// mismatch yields an empty result; the freshness check in
    /// [`git_history_if_fresh`](crate::mcp) has already gated this call, so the only way to get here
    /// with a dead daemon is a race, and an empty page is the safe answer.
    pub fn commits(&self, op: GitHistoryOp) -> Vec<CommitInfo> {
        match self.call(op) {
            Some(GitHistoryReply::Commits(commits)) => commits,
            _ => Vec::new(),
        }
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
        let client = self
            .client
            .get_or_try_init(|| async {
                let client = CommsClient::connect(
                    &crate::comms::singleton::resolve_paths()?,
                    self.agent.clone(),
                    None,
                    Some(self.root.clone()),
                )
                .await?;
                Ok::<_, crate::comms::client::CommsClientError>(Arc::new(Mutex::new(client)))
            })
            .await
            .inspect_err(|error| tracing::warn!(%error, "git-history: daemon unreachable; tools live-walk"))
            .ok()?;
        let mut guard = client.lock().await;
        guard
            .git_history(self.root.clone(), op)
            .await
            .inspect_err(|error| tracing::warn!(%error, "git-history: forwarded query failed; tools live-walk"))
            .ok()
    }
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
    crate::comms::singleton::probe_alive(&paths.socket_path)
}

/// Backoff schedule for the startup sync. The first session on a cold machine SPAWNS the daemon, and
/// a daemon that is still coming up answers nothing — a one-shot sync would then leave that session's
/// history tools live-walking for its entire life (the index would only get built by whoever came
/// next). Retry a handful of times, doubling, then give up.
const SYNC_RETRIES: u32 = 5;
const SYNC_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// Ask the daemon to bring `root`'s git-history index up to date, on a connection of its own.
///
/// Serve calls this once at startup (off the MCP thread) instead of building the index itself. The
/// dedicated connection matters: a first build on a deep repo runs for minutes, and it must not hold
/// the lock on the client this session's history queries use.
///
/// The daemon serializes syncs per repo and `builder::sync` is freshness-checked, so N sessions
/// asking at once produce ONE build; the losers of the race get [`SyncOutcome::Fresh`].
pub async fn request_sync(root: PathBuf, agent: AgentId) -> Option<SyncOutcome> {
    let mut backoff = SYNC_BACKOFF;
    for attempt in 0..=SYNC_RETRIES {
        match try_sync(&root, &agent, attempt == 0).await {
            Ok(outcome) => return Some(outcome),
            Err(error) if attempt == SYNC_RETRIES => {
                tracing::warn!(%error, "git-history: daemon sync failed; history tools live-walk");
            }
            Err(error) => {
                tracing::debug!(%error, ?backoff, "git-history: daemon sync failed; retrying");
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
        }
    }
    None
}

async fn try_sync(
    root: &std::path::Path,
    agent: &AgentId,
    may_spawn: bool,
) -> Result<SyncOutcome, crate::comms::client::CommsClientError> {
    let mut client = if may_spawn {
        CommsClient::ensure_and_connect(agent.clone(), None, Some(root.to_path_buf())).await?
    } else {
        CommsClient::connect(
            &crate::comms::singleton::resolve_paths()?,
            agent.clone(),
            None,
            Some(root.to_path_buf()),
        )
        .await?
    };
    match client.git_history(root.to_path_buf(), GitHistoryOp::Sync).await? {
        GitHistoryReply::Synced(outcome) => Ok(outcome),
        _ => Err(crate::comms::client::CommsClientError::Unexpected {
            request: "git_history sync",
        }),
    }
}
