//! Daemon-forwarding half of [`CommsClient`](super::client::CommsClient): the write/read RPCs a
//! `daemon_writer` serve ships to the daemon (the machine's sole fjall writer) rather than running
//! in-process — rescan and git-history.
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
        self.rescan_as(root, paths, full, embed, false).await
    }

    /// [`rescan`](Self::rescan), with `verify` set when `paths` were named by a person or an agent
    /// rather than seen changing by a watcher: the daemon then validates the blob of a named file
    /// it would skip as unchanged.
    pub async fn rescan_as(
        &mut self,
        root: PathBuf,
        paths: Option<Vec<PathBuf>>,
        full: bool,
        embed: bool,
        verify: bool,
    ) -> Result<RescanReport, CommsClientError> {
        match self
            .request(CommsRequest::Rescan {
                root,
                paths,
                full,
                embed,
                verify,
            })
            .await?
        {
            CommsResponse::Rescanned {
                scanned,
                updated,
                docs_indexed,
                docs_degraded,
                docs_text_fallback,
                doc_timeouts,
                removed,
                elapsed_ms,
            } => Ok(RescanReport {
                scanned,
                updated,
                docs_indexed,
                docs_degraded,
                docs_text_fallback,
                doc_timeouts,
                removed,
                elapsed_ms,
            }),
            other => Err(self.shape_err(other, "rescan")),
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
