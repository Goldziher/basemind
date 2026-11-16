//! What this binary can tell an MCP host about itself with no daemon to ask.
//!
//! `basemind serve` is a relay: the daemon answers the host. While the daemon cannot be reached,
//! most often because another basemind version still holds the workspace's index, the relay
//! answers `initialize` and `tools/list` from here, so the session starts and its tool calls can
//! say why they fail instead of the host giving the server up. The daemon the relay starts is this
//! same binary, so what is answered here is what that daemon answers. One the relay did not start
//! may differ (another release in the same generation, or one started with the lean surface),
//! which is why the capabilities here promise `tools.listChanged` and the relay sends that
//! notification once it connects.

use rmcp::ServerHandler;
use rmcp::model::{ErrorData, InitializeRequestParams, ListToolsResult, ServerConfig};
use serde_json::Value;

use super::{lean, router_cache, server_handler};

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

/// The `result` of `tools/list`: every tool this binary serves, or the lean wrappers when the lean
/// surface is on. Without the cache hints the daemon's answer carries, since this one is a
/// stand-in the host should let go of when told the list changed.
pub fn tools_list() -> Value {
    as_json(&tool_list())
}

fn tool_list() -> ListToolsResult {
    tool_list_for(lean::lean_mode_enabled())
}

/// [`tool_list`] for the lean surface or the full one.
fn tool_list_for(lean: bool) -> ListToolsResult {
    let tools = if lean {
        lean::lean_list_tools().tools
    } else {
        router_cache::cached_tool_router().list_all()
    };
    ListToolsResult::with_all_items(tools)
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

    /// Independent of `BASEMIND_MCP_LEAN` in the environment the tests run in: the full surface
    /// is asked for by name, and the answer sent is the list for the surface the switch selects.
    #[test]
    fn the_tool_list_is_the_advertised_one_without_cache_hints() {
        let listed = tool_list_for(false);
        assert!(listed.ttl_ms.is_none() && listed.cache_scope.is_none());
        let names: Vec<&str> = listed.tools.iter().map(|tool| tool.name.as_ref()).collect();
        for tool in crate::mcp::params::VERB_TOOLS {
            assert!(names.contains(tool), "`{tool}` is missing from {names:?}");
        }
        let (full, lean) = (as_json(&listed), as_json(&tool_list_for(true)));
        assert_ne!(full, lean, "the switch selects between two surfaces");
        let sent = tools_list();
        assert!(sent == full || sent == lean, "the list sent is one of them: {sent}");
    }
}
