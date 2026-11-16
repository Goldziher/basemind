//! The MCP server's identity: the principal of agent comms v2 (ADR-0021), resolved by the one
//! resolver `serve`, the CLI and the hooks share. It owns the session's individual memory notes.

/// This server's principal for the checkout at `root`. A root the resolver refuses (one that is
/// not absolute) falls back to the derived id of the directory as given, so memory still has an
/// owner.
pub(super) fn resolve_agent_id(root: &std::path::Path) -> String {
    match crate::comms::v2::identity::resolve_for_root(root) {
        Ok(resolved) => resolved.principal.as_str().to_string(),
        Err(error) => {
            tracing::warn!(%error, "agent identity: falling back to the default host");
            crate::comms::v2::identity::DEFAULT_HOST.to_string()
        }
    }
}
