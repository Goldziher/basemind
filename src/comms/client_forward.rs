//! Daemon-forwarding half of [`CommsClient`](super::client::CommsClient): the write/read RPCs a
//! `daemon_writer` serve ships to the daemon (the machine's sole fjall writer) rather than running
//! in-process — rescan, CORE memory, PROPOSAL governance, precise resolved-refs, and git-history.
//!
//! Split out of `client.rs` to keep it under the 1000-line `rust-max-lines` cap. These are a second
//! `impl CommsClient` block; the request/response correlation core stays in `client.rs`.

use std::path::PathBuf;

use super::client::{CommsClient, CommsClientError, RescanReport};
use super::protocol::{CommsRequest, CommsResponse};

impl CommsClient {
    /// Ask the daemon (the machine's sole fjall writer) to scan or rescan a workspace. Front-ends
    /// forward their writes here so concurrent read-only sessions never contend for the index lock.
    /// A non-empty `paths` (with `full == false`) drives an incremental rescan; otherwise the whole
    /// working tree is scanned. `embed` requests an
    /// [`EmbedMode::Inline`](crate::scanner::EmbedMode::Inline) vector-fill pass (documents + code
    /// chunks); `false` is the fast `Deferred` code-map pass. Idempotent, so the transparent
    /// reconnect-and-retry is replay-safe.
    pub async fn rescan(
        &mut self,
        root: PathBuf,
        paths: Option<Vec<PathBuf>>,
        full: bool,
        embed: bool,
    ) -> Result<RescanReport, CommsClientError> {
        match self
            .request(CommsRequest::Rescan {
                root,
                paths,
                full,
                embed,
            })
            .await?
        {
            CommsResponse::Rescanned {
                scanned,
                updated,
                removed,
                elapsed_ms,
            } => Ok(RescanReport {
                scanned,
                updated,
                removed,
                elapsed_ms,
            }),
            other => Err(self.shape_err(other, "rescan")),
        }
    }

    /// Forward a CORE memory operation to the daemon (the sole fjall writer). A `daemon_writer`
    /// serve resolves the namespace + scope and ships the op here; the vector (LanceDB) half stays
    /// serve-side. Idempotent for get/list/delete; `put` is a preserving RMW, so a replayed retry is
    /// safe.
    #[cfg(feature = "memory")]
    pub async fn memory_op(
        &mut self,
        root: PathBuf,
        scope: String,
        op: crate::comms::memory_proto::MemoryOp,
    ) -> Result<crate::comms::memory_proto::MemoryOutcome, CommsClientError> {
        match self.request(CommsRequest::Memory { root, scope, op }).await? {
            CommsResponse::Memory(outcome) => Ok(outcome),
            other => Err(self.shape_err(other, "memory_op")),
        }
    }

    /// Forward a PROPOSAL governance operation to the daemon (the sole fjall writer). A
    /// `daemon_writer` serve does the git-log mining / audit verdict / LanceDB embed on its side and
    /// ships only the fjall reads/writes here. Idempotent for list/get; reject + promote are
    /// terminal writes and mine-apply is tombstone-guarded, so a replayed retry is safe.
    #[cfg(feature = "memory")]
    pub async fn governance_op(
        &mut self,
        root: PathBuf,
        scope: String,
        op: crate::comms::proposals_proto::GovernanceOp,
    ) -> Result<crate::comms::proposals_proto::GovernanceOutcome, CommsClientError> {
        match self.request(CommsRequest::Governance { root, scope, op }).await? {
            CommsResponse::Governance(outcome) => Ok(outcome),
            other => Err(self.shape_err(other, "governance_op")),
        }
    }

    /// Forward a precise resolved-reference read to the daemon (the sole fjall writer, holding the
    /// cross-file `refs_by_def` / `refs_by_path` index a `daemon_writer` serve cannot see). Backs
    /// the precise cross-file `find_callers` / `goto_definition` path. A pure read, so the
    /// transparent reconnect-and-retry is replay-safe.
    pub async fn resolved_refs(
        &mut self,
        root: PathBuf,
        query: crate::comms::resolved_proto::ResolvedRefQuery,
    ) -> Result<crate::comms::resolved_proto::ResolvedRefResult, CommsClientError> {
        match self.request(CommsRequest::ResolvedRefs { root, query }).await? {
            CommsResponse::ResolvedRefs(result) => Ok(result),
            other => Err(self.shape_err(other, "resolved_refs")),
        }
    }

    /// Forward a git-history operation to the daemon — the sole holder of `git-history.fjall/`
    /// (fjall's directory lock is exclusive, so no front-end may open it). Backs both the index
    /// BUILD a `daemon_writer` serve requests at startup instead of running it in-process, and the
    /// history reads its `recent_changes` / `commits_touching` / `hot_files` / `search_git_history`
    /// tools would otherwise have to degrade to a live walk.
    ///
    /// Every op is idempotent — the reads are pure and `Sync` is freshness-checked (a replayed sync
    /// against an up-to-date index is a no-op) — so the transparent reconnect-and-retry is safe.
    pub async fn git_history(
        &mut self,
        root: PathBuf,
        op: crate::git_history::proto::GitHistoryOp,
    ) -> Result<crate::git_history::proto::GitHistoryReply, CommsClientError> {
        match self.request(CommsRequest::GitHistory { root, op }).await? {
            CommsResponse::GitHistory(reply) => Ok(reply),
            other => Err(self.shape_err(other, "git_history")),
        }
    }
}
