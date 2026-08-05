//! `expand` tool shim for `BasemindServer`.
//!
//! Thin wrapper; all logic lives in `super::helpers_compress`.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;

use super::BasemindServer;
use super::helpers::record_call;
use super::types_compress::ExpandParams;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_compress")]
impl BasemindServer {
    #[tool(
        output_schema = "rmcp::handler::server::tool::schema_for_output::<super::types_compress::ExpandResponse>()",
        description = "Return the full source body of one symbol resolved by path + name \
            (+ optional kind) from the L1 outline byte range. This is the companion to `compress`, \
            which returns signatures only: use `compress {path}` to get the outline of a file, \
            identify the symbol you need, then call `expand {path, name}` to pull just that \
            implementation into context — the context-offloading pattern. \
            Resolution: `name` is matched exactly (case-sensitive) against every symbol in the \
            file's L1 outline. When `name` alone matches multiple symbols (e.g. an overloaded \
            method), the tool returns an error listing the candidates; supply `kind` \
            (function/method/struct/enum/class/trait/type/const/module/macro) to disambiguate. \
            The body is the raw source slice `file_bytes[start_byte..end_byte]` from the L1 \
            outline. Bodies larger than 128 KiB are truncated and `truncated` is set to `true`. \
            Requires the file to be indexed; call `rescan` first if the path is not found.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn expand(&self, Parameters(p): Parameters<ExpandParams>) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(serde_json::Value::Null);
        let __result: Result<CallToolResult, McpError> = super::helpers_compress::run_expand(&self.state, p).await;
        record_call(&self.state, "expand", &__params_json, __started, &__result);
        __result
    }
}
