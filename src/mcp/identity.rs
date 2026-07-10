//! Stable agent-identity resolution for the MCP server.

use crate::store::Store;

/// File under `.basemind/` holding the generated-and-persisted per-session agent id. Created
/// the first time identity resolution falls through to the generated tier, so two `serve`
/// sessions against different repos get distinct ids while a single repo stays stable.
const AGENT_ID_FILE: &str = "agent-id";

/// Resolve this server's stable agent identity. Tiered, each candidate validated through
/// [`crate::comms::ids::AgentId`] (an invalid candidate falls through, not fails):
///
/// 1. `BASEMIND_AGENT_ID` env — explicit per-process override.
/// 2. `config.comms.agent_id` — workspace config.
/// 3. A generated-and-persisted id at `.basemind/agent-id` — stable per repo across restarts,
///    distinct across repos so two windows differ.
/// 4. `"anon"` — the final fallback (itself a valid `AgentId`).
///
/// ~keep TODO: prefer the MCP `clientInfo.name` from rmcp's `initialize` handshake once it is
/// cleanly reachable at construction time; the persisted per-session id is the stand-in.
pub(super) fn resolve_agent_id(config: &crate::config::Config, store: &Store) -> String {
    fn validated(candidate: Option<String>) -> Option<String> {
        candidate
            .and_then(|s| crate::comms::ids::AgentId::parse(s).ok())
            .map(|a| a.into_string())
    }

    if let Some(id) = validated(std::env::var("BASEMIND_AGENT_ID").ok()) {
        return id;
    }
    if let Some(id) = validated(config.comms.agent_id.clone()) {
        return id;
    }
    if let Some(id) = validated(load_or_create_persisted_agent_id(&store.basemind_dir)) {
        return id;
    }
    "anon".to_string()
}

/// Read the persisted per-session agent id from `<basemind_dir>/agent-id`, generating and
/// writing a fresh one when absent or unreadable. Best-effort: any io failure returns `None`
/// so the resolver falls through to `"anon"` rather than erroring at server boot.
fn load_or_create_persisted_agent_id(basemind_dir: &std::path::Path) -> Option<String> {
    let path = basemind_dir.join(AGENT_ID_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let token = format!("session-{:x}-{:x}", std::process::id(), nanos);
    let _ = std::fs::write(&path, &token);
    Some(token)
}
