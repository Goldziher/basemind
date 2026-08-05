//! In-process facade exposing the code-map `#[tool]` methods to non-MCP, in-crate-adjacent
//! callers — chiefly the `basemind-agent` engine.
//!
//! The `#[tool]` methods on [`BasemindServer`] are `pub(crate)` and take their arguments
//! wrapped in `Parameters<Lenient<_>>` (both `pub(crate)` machinery). A sibling crate can
//! therefore neither name the wrapper nor call the methods. Each function here bridges that
//! visibility: it accepts a plain `*Params` value, applies the wrapper, and calls the tool
//! method — running the identical code an MCP client would dispatch. Callers extract JSON
//! from the returned [`CallToolResult`] with [`crate::cli::render::result_to_value`].
//!
//! This mirrors the intent of the [`crate::mcp::params`] re-export module, which already
//! exists so the in-process CLI (`src/cli/`) can build tool arguments; the facade goes one
//! step further and also invokes the methods, so callers outside the crate need no access to
//! `pub(crate)` items at all.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;

use super::BasemindServer;
use super::lenient::Lenient;
use super::mode::GraphMode;
use super::params::{
    BlameSymbolParams, CallGraphParams, DiffFileParams, FindCallersParams, FindReferencesParams, GraphParams,
    OutlineParams, RecentChangesParams, SearchSymbolsParams, WorkspaceGrepParams,
};

/// Generate `pub async fn <name>(server, params) -> Result<CallToolResult, McpError>`
/// forwarders to the `pub(crate)` `#[tool]` methods. The `lenient` arm wraps the argument
/// in `Parameters<Lenient<_>>`; the `plain` arm wraps in `Parameters<_>` — matching each
/// method's actual signature.
macro_rules! facade {
    (lenient: $( $name:ident => $params:ty ),* $(,)?) => {
        $(
            #[doc = concat!("In-process invocation of the `", stringify!($name), "` code-map tool.")]
            pub async fn $name(server: &BasemindServer, params: $params) -> Result<CallToolResult, McpError> {
                server.$name(Parameters(Lenient(params))).await
            }
        )*
    };
    (plain: $( $name:ident => $params:ty ),* $(,)?) => {
        $(
            #[doc = concat!("In-process invocation of the `", stringify!($name), "` code-map tool.")]
            pub async fn $name(server: &BasemindServer, params: $params) -> Result<CallToolResult, McpError> {
                server.$name(Parameters(params)).await
            }
        )*
    };
}

facade! { lenient:
    outline => OutlineParams,
    search_symbols => SearchSymbolsParams,
    find_references => FindReferencesParams,
    find_callers => FindCallersParams,
    workspace_grep => WorkspaceGrepParams,
}

facade! { plain:
    recent_changes => RecentChangesParams,
    blame_symbol => BlameSymbolParams,
    diff_file => DiffFileParams,
}

/// In-process invocation of the `graph` tool's `calls` mode (formerly the `call_graph` tool).
///
/// Hand-written rather than generated: consolidation folded `call_graph` into the `graph` domain's
/// required `mode`, and this facade keeps the caller-facing signature — a plain [`CallGraphParams`]
/// in, the identical BFS body out — so the agent engine's tool wrapper is unaffected.
pub async fn call_graph(server: &BasemindServer, params: CallGraphParams) -> Result<CallToolResult, McpError> {
    let CallGraphParams {
        name,
        direction,
        path,
        max_depth,
        max_nodes,
    } = params;
    let graph = GraphParams {
        name: Some(name),
        direction: Some(direction),
        path,
        max_depth,
        max_nodes,
        ..GraphParams::new(GraphMode::Calls)
    };
    server.graph(Parameters(Lenient(graph))).await
}

/// Estimate the tokens a code-map tool call saved versus the shell/read baseline it replaces.
///
/// `tool` is the code-map tool name (e.g. `"outline"`, `"search_symbols"`) and `response_text` is
/// the JSON body returned to the model. Returns `0` for a tool with no disclosed baseline (a plain
/// `shell_exec` or `room_*` call), so callers can sum it unconditionally over every tool result.
/// Backs the agent TUI's "tokens saved" telemetry; see [`super::savings`] for the baseline model.
pub fn estimate_tokens_saved(tool: &str, response_text: &str) -> u64 {
    super::savings::estimate_from_text(tool, 0, response_text).est_tokens_saved
}
