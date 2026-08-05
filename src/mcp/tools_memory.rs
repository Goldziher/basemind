//! The `memory` domain tool shim for `BasemindServer`.
//!
//! One tool, one required `mode` — `put` / `get` / `list` / `search` / `delete` / `audit` /
//! `documents` / `mine` / `proposals` / `accept` / `reject` — dispatched to
//! `helpers_memory::run_memory`. The tool is registered in every build; the `memory` and
//! `documents` feature gates are enforced per mode inside the helper, so a binary without them
//! answers with "rebuild with --features …" instead of hiding the whole domain.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::lenient::Lenient;
use super::types_memory::MemoryParams;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_memory")]
impl BasemindServer {
    // No `output_schema`: the eleven modes return eleven different response shapes, and SEP-2106
    // allows exactly one per tool. Declaring a union would mean nested structs, which schemars
    // emits as `$ref` into `$defs` — the construct that silently dropped the whole registry in
    // GH #50. The per-mode shapes are documented in the description instead. ~keep
    #[tool(
        description = "Durable repo-scoped memory plus retrieval over indexed documents: remember \
        this, recall what we know, search the PDFs, and review mined suggestions. `mode` is \
        required. `put` writes (upserts) a note under a key so later sessions and other agents \
        read it back — `embed=false` skips vector indexing; `get` is an exact-key lookup, no \
        vector touch; `list` enumerates entries with a key-PREFIX filter (`prefix`, not substring) \
        and an exact `tag` filter, `cursor`-paged (default 100, max 1000); `search` is vector KNN \
        over stored notes — \"what do we already know about X\" (default 10, max 100, ranked by L2 \
        distance, `tag` filtered after the KNN); `delete` removes one key from both the index and \
        the vector store. `audit` re-verifies stored notes against the LIVE code index — file and \
        symbol provenance, structural-hash drift — decaying importance and archiving records stale \
        for over 90 days; `dry_run` previews the verdicts (default 100, max 1000). `documents` is \
        semantic search over indexed PDFs, Office files, HTML, email and OCR'd images — read the \
        matching chunks instead of opening the file; `mime_type` filters exactly and `scope` picks \
        the ingestion scope (pages ingested by the `web` tool live under `web:<host>`), \
        `max_tokens` budgets the hits and `format:\"toon\"` compacts them. `mine` derives \
        co-change proposals from git history (a candidate needs `min_support` co-changes and \
        `min_confidence` = support / anchor commits; bulk commits over `max_files_per_commit` are \
        skipped); `proposals` lists what is awaiting review; `accept` promotes one into searchable \
        memory tagged skill/cochange; `reject` tombstones it so mining never resurfaces it. \
        `visibility` selects the tier for the memory modes: `group` (shared across agents, the \
        default) or `individual` (private to the calling agent). Parameters that belong to another \
        mode are rejected, not ignored. Every mode except `documents` needs --features memory; \
        `documents` needs --features documents.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn memory(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<MemoryParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __key = p.mode.telemetry_key();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = super::helpers_memory::run_memory(&self.state, p).await;
        record_call(&self.state, __key, &__params_json, __started, &__result);
        __result
    }
}
