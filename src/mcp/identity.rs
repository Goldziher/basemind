//! Per-session agent-identity resolution for the MCP server.
//!
//! A thin adapter over [`crate::comms::identity`], which is the single source of truth shared with
//! the CLI verbs. The tiering itself lives there ON PURPOSE: `serve` and the CLI previously each
//! carried their own copy, drifted, and the CLI copies collapsed onto one hardcoded id for every
//! agent on the machine.

use crate::comms::identity::{self, AgentIdentity, IdentityPaths, IdentityRequest};
use crate::store::Store;

/// Resolve this server's routing identity. See [`crate::comms::identity`] for the tiers.
///
/// An explicit environment/config identity remains stable. Otherwise each live MCP server gets a
/// distinct session id, preventing concurrent same-workspace sessions from collapsing authorship.
pub(super) fn resolve_agent_id(config: &crate::config::Config, store: &Store) -> String {
    resolve_identity(config, store).into_id().into_string()
}

/// The full identity, including any cross-workspace claim collision. [`identity::resolve`] emits
/// the collision as a `tracing::warn!`, so `serve` surfaces it through its normal log sink.
fn resolve_identity(config: &crate::config::Config, store: &Store) -> AgentIdentity {
    identity::resolve_session(&IdentityRequest {
        root: &store.root,
        paths: IdentityPaths {
            workspace_cache_dir: store.basemind_dir.clone(),
            claims_dir: identity::claims_dir(),
        },
        env_agent_id: std::env::var(identity::AGENT_ID_ENV).ok(),
        config_agent_id: config.comms.agent_id.clone(),
    })
}
