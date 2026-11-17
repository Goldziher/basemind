//! The MCP server's identity: the principal of agent comms v2 (ADR-0021), resolved by the one
//! resolver `serve`, the CLI and the hooks share. It owns the session's individual memory notes.

/// This server's principal for the checkout at `root`. A root the resolver refuses (one that is
/// not absolute) falls back to the bare default host name, `agent`, so memory still has an owner.
pub(crate) fn resolve_agent_id(root: &std::path::Path) -> String {
    match crate::comms::v2::identity::resolve_for_root(root) {
        Ok(resolved) => resolved.principal.as_str().to_string(),
        Err(error) => {
            tracing::warn!(%error, "agent identity: no id can be derived for this root; memory notes are owned by `agent`");
            crate::comms::v2::identity::DEFAULT_HOST.to_string()
        }
    }
}
