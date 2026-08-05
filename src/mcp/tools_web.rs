//! The `web` domain tool shim for `BasemindServer`.
//!
//! One tool, one required `mode` — `scrape` / `crawl` / `map` — dispatched to
//! `helpers_web::run_web`. The whole module is gated on `feature = "crawl"`:
//! with the feature off the tool is never registered, so the agent does not see
//! it at all rather than seeing a `not_enabled` stub.

#![cfg(feature = "crawl")]

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::lenient::Lenient;
use super::types_web::WebParams;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_web")]
impl BasemindServer {
    // No `output_schema`: the three modes return three different response shapes, and SEP-2106
    // allows exactly one per tool. Declaring a union would mean nested structs, which schemars
    // emits as `$ref` into `$defs` — the construct that silently dropped the whole registry in
    // GH #50. The per-mode shapes are documented in the description instead. ~keep
    #[tool(
        description = "Pull the web into basemind: fetch a page, crawl a docs site, or list a \
        site's URLs. `mode` is required. `scrape` fetches one http/https URL, extracts markdown, \
        and chunks + embeds it into the documents vector store (scope `web:<host>`) so it is \
        retrievable later via `memory` mode `documents` — use it to pull a known spec, RFC, API \
        doc, changelog, or blog post into RAG instead of pasting the page into context; \
        `index=false` fetches metadata only and skips the embedding cost. `crawl` follows links \
        breadth-first from a seed URL and indexes every page under one shared scope — use it for a \
        section of a documentation site, not a single page; bounded by `[crawl].max_pages` / \
        `max_depth` in basemind.toml, with per-call overrides advisory. `map` discovers a site's \
        URLs from its sitemap and link map WITHOUT fetching bodies, returning lastmod / changefreq \
        / priority hints — use it to scope a follow-up `crawl` or to pick targeted `scrape` calls; \
        capped by `limit` (default 100, max 1000), with `total_urls` and `truncated` telling you \
        whether you are holding a page or the whole site. robots.txt is respected by default. \
        Parameters that belong to another mode are rejected, not ignored. Needs --features crawl.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn web(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<WebParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __key = p.mode.telemetry_key();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = super::helpers_web::run_web(&self.state, p).await;
        record_call(&self.state, __key, &__params_json, __started, &__result);
        __result
    }
}
