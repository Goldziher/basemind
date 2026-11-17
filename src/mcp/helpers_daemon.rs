//! The link a session keeps to the daemon for forwarded work: memory operations, resolved
//! references, index reads and the code-search lanes.
//!
//! The link names no agent: agent messaging does not go over it (ADR-0021). One link is opened
//! lazily per session and kept; a long forwarded operation (a rescan, an embed pass) opens its
//! own with [`connect_ephemeral_client`] so it does not hold up the reads queued on the shared one.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use tokio::sync::Mutex;

use super::ServerState;
use crate::comms::client::{ClientHello, CommsClient};

/// Upper bound on dialing (and, if need be, spawning) the daemon.
const CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);

/// Map a [`CommsClientError`](crate::comms::client::CommsClientError) into an MCP error with a
/// stable `comms:` prefix so agents can route on it.
pub(super) fn comms_err(error: impl std::fmt::Display) -> McpError {
    McpError::internal_error(format!("comms: {error}"), None)
}

/// Connect to the daemon without probing it from a request it hosts: a daemon-hosted session's
/// daemon is running by definition, and asking it to ensure itself would dial itself.
async fn connect(state: &ServerState) -> Result<CommsClient, McpError> {
    if state.host.is_some() {
        let paths = crate::comms::singleton::resolve_paths().map_err(comms_err)?;
        return CommsClient::connect(&paths, ClientHello::anonymous())
            .await
            .map_err(comms_err);
    }
    CommsClient::ensure_and_connect(ClientHello::anonymous())
        .await
        .map_err(comms_err)
}

/// This session's shared link to the daemon, connected on first use. A failed connect leaves
/// nothing cached, so the next call dials again.
pub(super) async fn daemon_client(state: &ServerState) -> Result<Arc<Mutex<CommsClient>>, McpError> {
    let handle = state
        .daemon_client
        .get_or_try_init(|| async {
            let client = tokio::time::timeout(CONNECT_BUDGET, connect(state))
                .await
                .map_err(|_| {
                    comms_err(format!(
                        "broker unresponsive: connect did not finish within {}s (retryable)",
                        CONNECT_BUDGET.as_secs()
                    ))
                })??;
            Ok::<_, McpError>(Arc::new(Mutex::new(client)))
        })
        .await?;
    Ok(Arc::clone(handle))
}

/// A link of its own for a long forwarded operation, owned and dropped by the caller.
pub(super) async fn connect_ephemeral_client(state: &ServerState) -> Result<CommsClient, McpError> {
    connect(state).await
}

/// Route a CORE memory op to the machine's sole fjall writer and return the outcome.
///
/// On the daemon-HOSTED path ([`ServerState`]'s shared `host` is `Some`) the writer pool is
/// in-process, so the op runs directly against it on a blocking thread — no socket loopback, which on
/// the daemon would be the daemon dialing itself. Every other `daemon_writer` serve has no host and
/// forwards over the socket. Both run the identical `run_memory_op` writer-side, so callers see one
/// outcome shape.
#[cfg(feature = "memory")]
pub(super) async fn dispatch_memory_op(
    state: &ServerState,
    op: crate::comms::memory_proto::MemoryOp,
) -> Result<crate::comms::memory_proto::MemoryOutcome, McpError> {
    if let Some(host) = &state.host {
        let host = Arc::clone(host);
        let root = state.shared.root.clone();
        let scope = state.shared.scope.clone();
        return tokio::task::spawn_blocking(move || host.host_memory(&root, &scope, op))
            .await
            .map_err(|error| McpError::internal_error(format!("host memory task panicked: {error}"), None))?
            .map_err(|error| McpError::internal_error(format!("host memory: {error}"), None));
    }
    let client = daemon_client(state).await?;
    let mut guard = client.lock().await;
    guard
        .memory_op(state.shared.root.clone(), state.shared.scope.clone(), op)
        .await
        .map_err(comms_err)
}
