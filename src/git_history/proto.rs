//! The wire types for the git-history RPC a `daemon_writer` serve forwards to the daemon.
//!
//! Fjall takes an exclusive per-directory process lock, so `git-history.fjall/` can only ever be
//! held by ONE process. Under the daemon-as-sole-writer model that process is the daemon: it builds
//! the index (the expensive walk) and answers the front-ends' history reads from it. A serve session
//! therefore holds no handle at all and speaks these ops over the socket instead.
//!
//! The op set is deliberately COARSE — one round trip per MCP history tool call, never per posting
//! or per commit — so the forwarded path stays a fixed ~IPC round trip instead of amortizing a
//! chatty point-read protocol.

use serde::{Deserialize, Serialize};

use super::builder::RebuildOutcome;
use super::fts::FtsScope;
use crate::git::CommitInfo;
use crate::path::RelPath;

/// One git-history operation, forwarded from a front-end to the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
// Adjacent tagging, matching `CommsRequest` / `CommsResponse`: the wire codec is msgpack
// (`rmp_serde`), which cannot round-trip an INTERNALLY tagged enum whose variants hold sequences.
#[serde(tag = "op", content = "args", rename_all = "snake_case")]
pub enum GitHistoryOp {
    /// Bring the repo's index up to date with HEAD (build / append / no-op), then report what was
    /// done. The daemon serializes this per repo, so N sessions asking at once produce ONE build.
    Sync,
    /// The HEAD the index is currently synced to, or `None` when it has never been built. The
    /// freshness key every history tool checks before it trusts the index.
    IndexedHead,
    /// Newest-first global commit log — backs `recent_changes`.
    RecentCommits {
        skip: usize,
        take: usize,
        include_files: bool,
    },
    /// Commits touching one path, newest-first — backs `commits_touching` / `blame`-adjacent walks.
    CommitsTouching { path: RelPath, skip: usize, take: usize },
    /// The newest `window` commits with files resolved — backs `hot_files` / `find_commits_by_path`.
    WindowCommits { window: usize },
    /// Full-text search over indexed commits — backs `search_git_history`.
    SearchCommits {
        query: String,
        scope: FtsScope,
        skip: usize,
        take: usize,
    },
}

/// The daemon's answer to a [`GitHistoryOp`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", content = "data", rename_all = "snake_case")]
pub enum GitHistoryReply {
    /// Reply to [`GitHistoryOp::Sync`].
    Synced(SyncOutcome),
    /// Reply to [`GitHistoryOp::IndexedHead`]: the 40-char hex sha, or `None` when never built.
    IndexedHead(Option<String>),
    /// Reply to every commit-returning op.
    Commits(Vec<CommitInfo>),
}

/// What a forwarded [`GitHistoryOp::Sync`] did. The owned mirror of
/// [`RebuildOutcome`](super::builder::RebuildOutcome) (whose `reason` is a `&'static str`, which
/// cannot round-trip through `Deserialize`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "data", rename_all = "snake_case")]
pub enum SyncOutcome {
    /// HEAD unchanged since the last sync — nothing to do. What every session AFTER the one that
    /// won the build race sees, and the proof that the daemon did not double-build.
    Fresh,
    /// Appended `added` commits reachable from the new HEAD.
    Incremental { added: u32 },
    /// Wiped and rebuilt from scratch, indexing `commits` commits.
    FullRebuild { reason: String, commits: u32 },
}

impl From<RebuildOutcome> for SyncOutcome {
    fn from(outcome: RebuildOutcome) -> Self {
        match outcome {
            RebuildOutcome::Fresh => SyncOutcome::Fresh,
            RebuildOutcome::Incremental { added } => SyncOutcome::Incremental { added },
            RebuildOutcome::FullRebuild { reason, commits } => SyncOutcome::FullRebuild {
                reason: reason.to_string(),
                commits,
            },
        }
    }
}
