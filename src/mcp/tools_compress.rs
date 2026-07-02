//! `compress` tool shim for `BasemindServer`.
//!
//! Thin wrapper; all logic lives in `super::helpers_compress`.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;

use super::BasemindServer;
use super::helpers::record_call;
use super::types_compress::{CheckpointParams, CompressParams, DeltaParams, DetectWasteParams, ExpandParams};

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_compress")]
impl BasemindServer {
    #[tool(
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

    #[tool(
        description = "Code-aware token compression. \
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
            `path` is not found.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn compress(&self, Parameters(p): Parameters<CompressParams>) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(serde_json::Value::Null);
        let __result: Result<CallToolResult, McpError> = super::helpers_compress::run_compress(&self.state, p).await;
        record_call(&self.state, "compress", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Compute a compact +N/-M line-diff from `old` to `new` — the stateless \
            re-read primitive: when re-reading content you've already seen, emit only what \
            changed instead of the full text. Both sides are supplied inline (unlike the CLI, \
            which reads `old` from a file). Identical inputs return `changed=false` with a \
            `# unchanged` marker. Either side over 50,000 bytes or 2,000 lines bails to a full \
            re-read: `bailed=true`, `output` carries the NEW content verbatim behind a marker, \
            and `added`/`removed` are 0. Otherwise an LCS line diff runs and `output` is a \
            `+A/-R` header followed by `-`/`+` lines for the changed regions only; common lines \
            are omitted. Pure and stateless — it does not know what the caller has previously \
            seen; the caller supplies both sides.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn delta(&self, Parameters(p): Parameters<DeltaParams>) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(serde_json::Value::Null);
        let __result: Result<CallToolResult, McpError> = super::helpers_compress::run_delta(&self.state, p).await;
        record_call(&self.state, "delta", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Extract decisions, errors, and changed files from session `text` (a \
            transcript chunk or concatenated tool output) into a compact, credential-safe \
            checkpoint — persist or re-inject this instead of the whole session. \
            `decisions` are lines matching conservative decision markers (decided/chose/\
            \"we will\"/\"going with\"/opted for/conclusion:/TODO/FIXME); `errors` are lines \
            matching the error-line heuristic. `files_changed` comes from THIS server's git \
            working tree (staged + modified + untracked paths at call time) — never scraped \
            from `text`. Any candidate line that embeds a credential (API key, token, etc.) is \
            dropped entirely from every field before it can be returned. Each list is capped \
            (decisions/errors 50, files 200); a list that exceeded its cap sets the matching \
            `*_truncated` flag. Fails open on git: if this server is not running inside a git \
            repository (or git errors), `files_changed` is simply empty — checkpoint never \
            errors because the working tree could not be read.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn checkpoint(
        &self,
        Parameters(p): Parameters<CheckpointParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(serde_json::Value::Null);
        let __result: Result<CallToolResult, McpError> = super::helpers_compress::run_checkpoint(&self.state, p).await;
        record_call(&self.state, "checkpoint", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Flag redundant reads, repeated queries, and oversized reads from a \
            JSON-Lines tool-call `log` (one `{\"tool\", \"target\", \"bytes\"}` record per \
            line; malformed or `tool`-less lines are silently skipped). Three detectors, each \
            producing a `WasteFinding {kind, target, count, estimated_waste_bytes}`: \
            `redundant_read` — a `Read` `target` (file path) seen 2+ times, waste = bytes of \
            every read after the first; `repeated_query` — a search/grep tool (Grep, \
            workspace_grep, search_symbols, find_references, grep) with an identical `target` \
            (query string) 2+ times, same waste accounting; `oversized_read` — any single \
            `Read` with `bytes >= 32768` (suggesting `outline`/`search_symbols` instead of a \
            full read). Findings are sorted deterministically by `(kind, target)` and capped at \
            200 (`truncated=true` past the cap; `total_estimated_waste_bytes` still sums every \
            finding found, before truncation). A finding whose `target` embeds a credential is \
            dropped entirely. Pure analysis — this tool executes nothing and holds no state \
            across calls.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn detect_waste(
        &self,
        Parameters(p): Parameters<DetectWasteParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(serde_json::Value::Null);
        let __result: Result<CallToolResult, McpError> =
            super::helpers_compress::run_detect_waste(&self.state, p).await;
        record_call(&self.state, "detect_waste", &__params_json, __started, &__result);
        __result
    }
}
