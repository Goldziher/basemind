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
use super::params::{
    BlameSymbolParams, CallGraphParams, DiffFileParams, FindCallersParams, FindReferencesParams, OutlineParams,
    RecentChangesParams, SearchSymbolsParams, WorkspaceGrepParams,
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
    call_graph => CallGraphParams,
    recent_changes => RecentChangesParams,
    blame_symbol => BlameSymbolParams,
    diff_file => DiffFileParams,
}
