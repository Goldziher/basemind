//! The index verb tools of `BasemindServer`: `index_status` and `index_rescan` (ADR-0018).
//!
//! `index_rescan` is the only code tool that writes. Telemetry has no MCP tool and is reached
//! through `basemind admin telemetry`; the cache operations run offline in `basemind cache …`. See
//! `tools.rs` for the shim contract.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;

use super::BasemindServer;
use super::strict::Strict;
use super::types::TelemetrySummaryParams;
use super::types_tools::{IndexRescanParams, IndexStatusParams};
use crate::engine::Workspace;
use crate::engine::ops::index::RescanProgress;
use crate::engine::types::index::StatusResponse;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_admin")]
impl BasemindServer {
    #[tool(
        title = "Index status",
        description = "Use when a result is unexpectedly empty or says \"not indexed\": reports how \
        many files are indexed per language, whether a scan is running or the index is degraded, \
        the cache directory, and the repository's branch and HEAD. \
        Example: {}",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn index_status(
        &self,
        Parameters(args): Parameters<Strict<IndexStatusParams>>,
    ) -> Result<CallToolResult, McpError> {
        self.answer(args, |p| self.index_status_call(p)).await
    }

    #[tool(
        title = "Re-index",
        description = "Use after editing files, or when the index is empty or stale: re-indexes the \
        given `paths`, or with no arguments the whole workspace, skipping unchanged files. Writes \
        only basemind's cache, never the repository. \
        Example: {\"paths\":[\"src/store.rs\"]}",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn index_rescan(
        &self,
        Parameters(args): Parameters<Strict<IndexRescanParams>>,
        peer: rmcp::Peer<rmcp::RoleServer>,
        meta: rmcp::model::RequestMetaObject,
    ) -> Result<CallToolResult, McpError> {
        let progress = Some((&peer, meta.get_progress_token()));
        self.answer(args, |p| self.index_rescan_with(p, progress)).await
    }
}

impl BasemindServer {
    /// Body of `index_status`, shared with `basemind status`: the engine's report with this server's
    /// admission lanes, read before the call (which takes no slot) runs.
    pub(crate) async fn index_status_call(&self, p: IndexStatusParams) -> Result<CallToolResult, McpError> {
        let admission = self.heavy_admission.status();
        super::adapter::call_unpaged(&self.state, p, move |ws: &Workspace, p| {
            ws.index_status(p).map(|status| StatusResponse {
                admission: Some(admission),
                ..status
            })
        })
        .await
    }

    /// Body of `index_rescan`: [`Workspace::index_rescan`], whose scan runs in the daemon's pool for
    /// a hosted workspace ([`daemon_forward::writer_rescan`](super::daemon_forward::writer_rescan)).
    /// `basemind rescan` reaches the same scanner through its own lock-aware driver
    /// (`src/main_rescan.rs`), which also owns the daemon hand-off.
    ///
    /// Re-indexing is one of the few genuinely slow operations, so it reports MCP progress when the
    /// client supplies a token. The scan reports on a blocking thread; the notifications go out from
    /// this task, which is the one a task-capable client's `tasks/get` status mirrors.
    async fn index_rescan_with(
        &self,
        p: IndexRescanParams,
        progress: Progress<'_>,
    ) -> Result<CallToolResult, McpError> {
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        #[cfg(all(feature = "comms", any(unix, windows)))]
        let pool = self.state.pool.clone();
        let call = super::adapter::call_unpaged(&self.state, p, move |ws: &Workspace, p| {
            let mut progress = ChannelProgress(sender);
            #[cfg(all(feature = "comms", any(unix, windows)))]
            if let Some(pool) = &pool {
                return ws.index_rescan(p, &mut progress, |ws, paths| {
                    super::daemon_forward::writer_rescan(ws, pool, paths, false, true, true)
                });
            }
            ws.index_rescan(p, &mut progress, Workspace::rescan_own_store)
        });
        tokio::pin!(call);
        let result = loop {
            tokio::select! {
                biased;
                Some(event) = events.recv() => emit(&progress, event).await,
                result = &mut call => break result,
            }
        };
        while let Ok(event) = events.try_recv() {
            emit(&progress, event).await;
        }
        result
    }

    /// Usage and token-savings summary. No MCP tool: `basemind admin telemetry`.
    pub(crate) async fn telemetry_call(&self, p: TelemetrySummaryParams) -> Result<CallToolResult, McpError> {
        super::helpers::run_telemetry_summary(&self.state, p).await
    }
}

/// Where `index_rescan` reports progress: the calling peer and the progress token it sent, if any.
type Progress<'a> = Option<(&'a rmcp::Peer<rmcp::RoleServer>, Option<rmcp::model::ProgressToken>)>;

/// A step of a running rescan, sent from the blocking thread the scan runs on.
enum RescanEvent {
    Started,
    Finished(usize),
}

/// [`RescanProgress`] over a channel to the task that answers the call.
struct ChannelProgress(tokio::sync::mpsc::UnboundedSender<RescanEvent>);

impl RescanProgress for ChannelProgress {
    fn started(&mut self) {
        let _ = self.0.send(RescanEvent::Started);
    }

    fn finished(&mut self, scanned: usize) {
        let _ = self.0.send(RescanEvent::Finished(scanned));
    }
}

/// Send `event` as an MCP progress notification, when the client asked for them.
async fn emit(progress: &Progress<'_>, event: RescanEvent) {
    let Some((peer, Some(token))) = progress else {
        return;
    };
    match event {
        RescanEvent::Started => {
            super::notifications::emit_progress(peer, token.clone(), 0.0, None, "rescan: scanning working tree").await;
        }
        RescanEvent::Finished(scanned) => {
            let total = scanned as f64;
            super::notifications::emit_progress(
                peer,
                token.clone(),
                total,
                Some(total),
                format!("rescan: done, {scanned} files"),
            )
            .await;
        }
    }
}
