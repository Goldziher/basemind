//! What this binary can tell an MCP host about itself with no daemon to ask.
//!
//! `basemind serve` is a relay: the daemon answers the host. While the daemon cannot be reached,
//! most often because another basemind version still holds the workspace's index, the relay
//! answers `initialize` and `tools/list` from here, so the session starts and its tool calls can
//! say why they fail instead of the host giving the server up. The daemon the relay starts is this
//! same binary, so what is answered here is what that daemon answers. One the relay did not start
//! may differ (another release in the same generation), which is why the capabilities here promise `tools.listChanged` and the relay sends that
//! notification once it connects.

use rmcp::ServerHandler;
use rmcp::model::{ErrorData, InitializeRequestParams, ListToolsResult, ServerConfig};
use serde_json::Value;

use super::{router_cache, server_handler};

/// The server as it describes itself before any workspace is open.
struct Unconnected;

impl ServerHandler for Unconnected {
    fn get_info(&self) -> ServerConfig {
        let mut info = server_handler::server_config();
        info.capabilities.tools.get_or_insert_default().list_changed = Some(true);
        info
    }
}

/// The `result` of an `initialize` request with these `params`, negotiated as the daemon would;
/// or the JSON-RPC `error` object when the params cannot be read or no protocol version fits.
pub fn initialize(params: Value) -> Result<Value, Value> {
    let request: InitializeRequestParams =
        serde_json::from_value(params).map_err(|error| as_json(&ErrorData::invalid_params(error.to_string(), None)))?;
    match Unconnected.negotiate_initialize(&request) {
        Ok(result) => Ok(as_json(&result)),
        Err(error) => Err(as_json(&error)),
    }
}

/// The `result` of `tools/list`: every tool this binary serves. Without the cache hints the daemon's answer carries, since this one is a
/// stand-in the host should let go of when told the list changed.
pub fn tools_list() -> Value {
    as_json(&tool_list())
}

fn tool_list() -> ListToolsResult {
    ListToolsResult::with_all_items(router_cache::cached_tool_router().list_all())
}

/// The always-loaded verb tools this binary annotates `readOnlyHint: true`, in `VERB_TOOLS` order.
/// Read from the router's annotations, so a tool that writes is never listed, whatever its name.
pub fn read_only_verb_tools() -> Vec<&'static str> {
    let router = router_cache::cached_tool_router();
    super::params::VERB_TOOLS
        .iter()
        .copied()
        .filter(|name| {
            router
                .get(name)
                .and_then(|tool| tool.annotations.as_ref())
                .and_then(|annotations| annotations.read_only_hint)
                == Some(true)
        })
        .collect()
}

fn as_json(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(version: &str) -> Value {
        serde_json::json!({
            "protocolVersion": version,
            "capabilities": {},
            "clientInfo": { "name": "host", "version": "1" },
        })
    }

    #[test]
    fn initialize_is_answered_as_the_server_would_and_promises_a_tool_list_change() {
        let result = initialize(params("2025-03-26")).expect("an initialize result");
        assert_eq!(
            result["protocolVersion"], "2025-03-26",
            "the host's version is honoured"
        );
        assert_eq!(result["capabilities"]["tools"]["listChanged"], true, "{result}");
        let served = as_json(&server_handler::server_config());
        assert_eq!(result["serverInfo"], served["serverInfo"]);
        assert_eq!(result["instructions"], served["instructions"]);
        for capability in ["prompts", "resources", "completions"] {
            assert_eq!(
                result["capabilities"][capability], served["capabilities"][capability],
                "{capability}"
            );
        }
    }

    #[test]
    fn initialize_params_that_cannot_be_read_get_an_invalid_params_error() {
        let error = initialize(serde_json::json!({})).expect_err("no protocol version, no client");
        assert_eq!(error["code"], -32602, "{error}");
    }

    #[test]
    fn the_tool_list_is_the_advertised_one_without_cache_hints() {
        let listed = tool_list();
        assert!(listed.ttl_ms.is_none() && listed.cache_scope.is_none());
        let names: Vec<&str> = listed.tools.iter().map(|tool| tool.name.as_ref()).collect();
        for tool in crate::mcp::params::VERB_TOOLS {
            assert!(names.contains(tool), "`{tool}` is missing from {names:?}");
        }
        assert_eq!(tools_list(), as_json(&listed), "the list sent is the advertised one");
    }

    #[test]
    fn the_read_only_tools_match_the_table_the_cli_reads() {
        assert_eq!(read_only_verb_tools(), crate::engine::tool_names::READ_ONLY_VERB_TOOLS);
    }

    #[test]
    fn the_read_only_tools_leave_out_every_tool_that_writes() {
        let read_only = read_only_verb_tools();
        for writer in ["memory_save", "index_rescan"] {
            assert!(!read_only.contains(&writer), "`{writer}` writes: {read_only:?}");
        }
        assert!(
            !read_only.iter().any(|name| name.starts_with("agent_")),
            "{read_only:?}"
        );
        assert_eq!(
            read_only.len(),
            crate::mcp::params::VERB_TOOLS.len() - 2,
            "{read_only:?}"
        );
    }
}
