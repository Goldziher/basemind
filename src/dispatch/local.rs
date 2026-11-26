//! The in-process engine handle a `basemind` command runs its operation on.
//!
//! One one-shot workspace with every background facility disabled: no auto-scan, no watcher, no
//! background GC, so the process exits the instant the single call returns. The in-RAM map is built
//! lazily, on the first call that reads it. The store is opened read-only by the caller, so a command
//! never contends for the lock a running `basemind serve` may hold.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde::Serialize;

use super::{Op, json_result, paged_call, plain_call, task_failed, unpaged_call};
use crate::config::Config;
use crate::engine::paging::{Paged, ToolArgs};
use crate::engine::telemetry::{self, Telemetry};
use crate::engine::types::index::{AdmissionStatus, IndexStatusParams, StatusResponse};
use crate::engine::types::search::{CodeSearchParams, DocsSearchParams};
use crate::engine::types::telemetry::TelemetrySummaryParams;
use crate::engine::{EngineError, Workspace};
use crate::git::Repo;
use crate::git_cache::GitCache;
use crate::store::Store;

/// A one-shot workspace and the telemetry writer its calls record to.
pub(crate) struct LocalEngine {
    workspace: Arc<Workspace>,
    telemetry: Arc<Telemetry>,
}

impl LocalEngine {
    /// Open the one-shot workspace over `store`. `read_only` is set when another process (the
    /// daemon) holds the index, so this one answers from it and writes nothing.
    pub(crate) fn open(
        store: Store,
        root: PathBuf,
        config: Arc<Config>,
        repo: Option<Arc<Repo>>,
        git_cache: Arc<GitCache>,
        read_only: bool,
    ) -> Self {
        let history_dir = crate::git_history::shared_history_basemind_dir(&root);
        let git_history = super::open_git_history(&history_dir, repo.is_some(), false, read_only);
        let telemetry = Arc::new(Telemetry::new(&store.basemind_dir));
        let workspace = Arc::new(Workspace::new(
            store,
            root,
            config,
            repo,
            git_cache,
            git_history,
            super::open_options(false, read_only, true),
        ));
        Self { workspace, telemetry }
    }

    /// The repository root this engine serves.
    pub(crate) fn root(&self) -> &Path {
        &self.workspace.root
    }

    /// Run the engine operation `op` as a verb-tool call: cursor binding, the telemetry row and the
    /// tool's wire encoding, as the MCP server answers it.
    pub(crate) async fn op<P, R>(
        &self,
        p: P,
        op: impl FnOnce(&Workspace, P) -> Result<R, EngineError> + Send + 'static,
    ) -> Result<CallToolResult, McpError>
    where
        P: Paged + Send + 'static,
        R: Serialize + 'static,
    {
        let (ws, telemetry) = (Arc::clone(&self.workspace), Arc::clone(&self.telemetry));
        blocking(move || paged_call(&ws, &telemetry, p, op)).await
    }

    /// [`op`](Self::op) for a verb tool that pages nothing: no cursor to bind, the same telemetry row.
    pub(crate) async fn op_unpaged<P, R>(
        &self,
        p: P,
        op: impl FnOnce(&Workspace, P) -> Result<R, EngineError> + Send + 'static,
    ) -> Result<CallToolResult, McpError>
    where
        P: ToolArgs + Serialize + Send + 'static,
        R: Serialize + 'static,
    {
        let (ws, telemetry) = (Arc::clone(&self.workspace), Arc::clone(&self.telemetry));
        blocking(move || unpaged_call(&ws, &telemetry, p, op)).await
    }

    /// Run the engine operation `op` for a CLI-only command (`git churn`, `code chunk`): no MCP tool,
    /// so no cursor binding and no telemetry row.
    pub(crate) async fn cli_op<P, R>(&self, p: P, op: Op<P, R>) -> Result<CallToolResult, McpError>
    where
        P: Send + 'static,
        R: Serialize + 'static,
    {
        let ws = Arc::clone(&self.workspace);
        blocking(move || plain_call(&ws, p, op)).await
    }

    /// `code_search`, which nothing cancels in a one-shot command.
    pub(crate) async fn code_search(&self, p: CodeSearchParams) -> Result<CallToolResult, McpError> {
        self.op(p, |ws: &Workspace, p| ws.code_search(p, &|| false)).await
    }

    /// `docs_search`, which nothing cancels in a one-shot command.
    pub(crate) async fn docs_search(&self, p: DocsSearchParams) -> Result<CallToolResult, McpError> {
        self.op(p, |ws: &Workspace, p| ws.docs_search(p, &|| false)).await
    }

    /// `index_status`, reporting the admission lanes of a process that serves nobody else: idle.
    pub(crate) async fn index_status(&self, p: IndexStatusParams) -> Result<CallToolResult, McpError> {
        let admission = AdmissionStatus::idle(crate::config::engine::with(|engine| engine.admission));
        self.op_unpaged(p, move |ws: &Workspace, p| {
            ws.index_status(p).map(|status| StatusResponse {
                admission: Some(admission),
                ..status
            })
        })
        .await
    }

    /// The usage and token-savings summary of this workspace's telemetry log. It is not a tool call,
    /// so it records no row.
    pub(crate) async fn telemetry_summary(&self, p: TelemetrySummaryParams) -> Result<CallToolResult, McpError> {
        let telemetry = Arc::clone(&self.telemetry);
        blocking(move || {
            let summary = telemetry::summarize(telemetry.path(), p)?;
            json_result(&summary)
        })
        .await
    }
}

/// Run `body` on a blocking thread: an operation reads the store and the disk synchronously, which
/// must not park a runtime worker. A panic in it comes back as an internal error.
async fn blocking(
    body: impl FnOnce() -> Result<CallToolResult, McpError> + Send + 'static,
) -> Result<CallToolResult, McpError> {
    tokio::task::spawn_blocking(body)
        .await
        .unwrap_or_else(|join| Err(task_failed(&join)))
}
