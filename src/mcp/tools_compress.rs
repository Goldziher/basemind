//! `compress` tool shim for `BasemindServer`.
//!
//! Thin wrapper; all logic lives in `super::helpers_compress`.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;

use super::BasemindServer;
use super::helpers::record_call;
use super::types_compress::CompressParams;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_compress")]
impl BasemindServer {
    #[tool(description = "Code-aware token compression. \
            For indexed source files (supply `path`): returns the L1 structural outline \
            (imports + symbol signatures) from the code map — bodies are never included. \
            This is lossless for navigation purposes: signatures are returned verbatim, \
            never paraphrased. Strategy is always `structural` for code files. \
            For prose text (supply `text`): applies a lexical pass — collapses whitespace \
            runs, removes common English filler phrases, and deduplicates repeated paragraphs. \
            Strategy is `lexical`. \
            Exactly one of `text` or `path` must be supplied; both or neither is an error. \
            `level` (off|light|moderate|aggressive|maximum; default moderate) is accepted \
            but currently ignored in the V1 implementation — reserved for the prose tier. \
            `preserve_code` (default true) is similarly reserved. \
            `target_tokens` is a soft budget hint echoed in the response but does not \
            hard-cap output. \
            Token counts are estimated as bytes/4 (disclosed in `tokens_note`). \
            The structural path requires the file to be indexed; call `rescan` first if \
            `path` is not found.")]
    pub(crate) async fn compress(
        &self,
        Parameters(p): Parameters<CompressParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(serde_json::Value::Null);
        let __result: Result<CallToolResult, McpError> =
            super::helpers_compress::run_compress(&self.state, p).await;
        record_call(
            &self.state,
            "compress",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }
}
