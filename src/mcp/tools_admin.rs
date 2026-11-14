//! The `admin` domain tool shim for `BasemindServer`.
//!
//! One tool, one required `mode` — index health, re-indexing, cache accounting and telemetry —
//! dispatched to `helpers_admin::run_admin`. Thin wrapper: the bodies live in `helpers_admin.rs`.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::lenient::Lenient;
use super::types_admin::AdminParams;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_admin")]
impl BasemindServer {
    // No `output_schema`: the seven modes return seven different response shapes, and SEP-2106
    // allows exactly one per tool. Declaring a union would mean nested structs, which schemars
    // emits as `$ref` into `$defs` — the construct that silently dropped the whole registry in
    // GH #50. The per-mode shapes are documented in the description instead. ~keep
    #[tool(
        title = "Admin",
        description = "Operate basemind itself: index health, re-indexing, cache footprint and \
        cleanup, and usage telemetry. `mode` is required. `status` \
        reports this workspace's indexed state — file count, per-language breakdown, total bytes, \
        on-disk blob count, schema version, and whether a scan is still running; ask it first when \
        a query comes back empty or says 'no index' / 'no indexed files'. `repo` returns repository \
        identity: workdir, current branch, full + short HEAD sha. `rescan` re-indexes the working \
        tree in-process so symbols, calls, and outlines you just wrote become searchable — run it \
        after editing code; `paths` scopes it incrementally, `full: true` forces a complete \
        re-index and overrides `paths`. It holds an exclusive lock (other queries block; <1s for \
        ~100 files) and returns scanned / updated / removed counts. `cache_stats` reports \
        basemind's resource footprint: on-disk bytes per component (blobs / views / lance / \
        git-cache / telemetry / git-history), the `du`-accurate `total_bytes`, blob and orphan \
        counts, the serving process's current + peak RSS, and `last_gc` (outcome of the last \
        destructive sweep). `gc` reports how much of the blob store is orphaned and reclaimable \
        — report-only over MCP, `removed` is always 0; the CLI `basemind cache gc` performs the \
        cross-workspace sweep. `cache_clear` deletes one cache component (`component`: \
        blobs|views|lance|git-cache|telemetry|all, or views:<name>); `blobs` needs `confirm=true`, \
        and `views`/`all` are refused in-process because they back the live index — stop the server \
        and run `basemind cache clear` instead. `telemetry` aggregates recorded calls into a usage \
        summary — total calls, per-tool histogram, response bytes, heuristic `est_tokens_saved` — \
        over `window` (`today` default, `1h`, `24h`, `all`), optionally filtered to one `tool`. \
        Parameters that belong to another mode are rejected, not \
        ignored.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn admin(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<AdminParams>>,
        peer: rmcp::Peer<rmcp::RoleServer>,
        meta: rmcp::model::RequestMetaObject,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __key = p.mode.telemetry_key();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __progress = Some((&peer, meta.get_progress_token()));
        let __result: Result<CallToolResult, McpError> =
            super::helpers_admin::run_admin(std::sync::Arc::clone(&self.state), p, __progress).await;
        record_call(&self.state, __key, &__params_json, __started, &__result);
        __result
    }
}

impl BasemindServer {
    /// The `admin` tool for a caller that has no MCP peer — the CLI's entry point.
    ///
    /// `rescan` is the only mode that notifies a peer (progress + a completion log), and a one-shot
    /// `basemind admin …` process has nobody to notify; every mode otherwise runs the identical
    /// body the `#[tool]` shim runs, telemetry included.
    pub(crate) async fn admin_cli(&self, p: AdminParams) -> Result<CallToolResult, McpError> {
        let started = std::time::Instant::now();
        let key = p.mode.telemetry_key();
        let params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let result = super::helpers_admin::run_admin(std::sync::Arc::clone(&self.state), p, None).await;
        record_call(&self.state, key, &params_json, started, &result);
        result
    }
}
