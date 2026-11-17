//! How a session that does not hold the index reaches the one that does: a rescan over a link of
//! its own to the daemon, and a memory operation through the daemon's pool, in process, for a
//! daemon-hosted session.
//!
//! The link names no agent: agent messaging does not go over it (ADR-0021).

#![cfg(all(feature = "comms", any(unix, windows)))]

#[cfg(feature = "memory")]
use std::sync::Arc;

use rmcp::ErrorData as McpError;

use super::ServerState;
use crate::comms::client::{ClientHello, CommsClient};

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

/// A link of its own for a long forwarded operation, owned and dropped by the caller.
pub(super) async fn connect_ephemeral_client(state: &ServerState) -> Result<CommsClient, McpError> {
    connect(state).await
}

/// Run a CORE memory op against the machine's sole fjall writer: the daemon's pool, in process, on
/// a blocking thread. Only a daemon-hosted session forwards memory; a CLI next to the daemon
/// forwards its whole call instead.
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
    Err(McpError::internal_error(
        "memory: this session is neither hosted by the daemon nor holds the index",
        None,
    ))
}
