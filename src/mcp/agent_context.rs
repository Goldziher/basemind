//! Who a daemon-hosted session is to the agent tools (ADR-0021): the store it reads and writes,
//! the principal its relay named, and the repository of its root.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::sync::{Arc, Mutex, PoisonError};

use crate::comms::agents_hub::{AgentHub, ServeSession};
use crate::comms::v2::api::{Principal, RepoKey};

/// The agent identity of one daemon-hosted MCP session, counted as a live `serve` session of its
/// principal for as long as it lives.
pub struct AgentContext {
    /// The daemon's agent store.
    pub hub: Arc<AgentHub>,
    /// The principal the session's relay resolved.
    pub principal: Principal,
    /// The repository of the session's root.
    pub repo: RepoKey,
    /// Notices for the session's first agent-tool reply.
    notices: Mutex<Vec<String>>,
    _session: ServeSession,
}

impl AgentContext {
    /// The context of a session acting as `principal` in `repo`.
    pub fn new(hub: Arc<AgentHub>, principal: Principal, repo: RepoKey, notices: Vec<String>) -> Self {
        let session = hub.serve_session(principal.clone());
        Self {
            hub,
            principal,
            repo,
            notices: Mutex::new(notices),
            _session: session,
        }
    }

    /// The identity notices, once: the first agent-tool reply carries them.
    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock().unwrap_or_else(PoisonError::into_inner))
    }
}
