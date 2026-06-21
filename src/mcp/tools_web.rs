//! Web ingestion tool shims for `BasemindServer`.
//!
//! Each shim is a thin wrapper around its `run_*` helper in `helpers_web.rs`,
//! with telemetry instrumentation matching the rest of the MCP surface. The
//! whole module is gated on `feature = "crawl"` — when the feature is off,
//! these tools are never registered on the server, and the agent will not see
//! them in the tool list at all (rather than seeing a `not_enabled` stub).

#![cfg(feature = "crawl")]

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::types::{WebCrawlParams, WebMapParams, WebScrapeParams};

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_web")]
impl BasemindServer {
    #[tool(
        description = "Fetch one http/https URL, extract markdown, chunk + embed into the documents \
        vector store (scope `web:<host>`). Respects robots.txt by default. `index=false` fetches \
        metadata only, skipping embedding. Use to pull a known doc/spec/blog post into RAG. \
        Needs --features crawl."
    )]
    pub(crate) async fn web_scrape(
        &self,
        Parameters(p): Parameters<WebScrapeParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> =
            super::helpers_web::run_web_scrape(&self.state, p).await;
        record_call(
            &self.state,
            "web_scrape",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Crawl a site from `url` to the configured depth; index each page into the \
        documents vector store under one shared scope. Bounded by `[crawl].max_pages` / \
        `max_depth` in basemind.toml (per-call overrides advisory). Respects robots.txt by \
        default. Use for a section of a docs site, not a single page. Needs --features crawl."
    )]
    pub(crate) async fn web_crawl(
        &self,
        Parameters(p): Parameters<WebCrawlParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> =
            super::helpers_web::run_web_crawl(&self.state, p).await;
        record_call(
            &self.state,
            "web_crawl",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Discover URLs on a site via sitemap + link map without fetching page bodies. \
        Returns each URL with lastmod / changefreq / priority hints when present. Use to scope a \
        follow-up `web_crawl` or pick targeted `web_scrape` calls. Needs --features crawl."
    )]
    pub(crate) async fn web_map(
        &self,
        Parameters(p): Parameters<WebMapParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> =
            super::helpers_web::run_web_map(&self.state, p).await;
        record_call(&self.state, "web_map", &__params_json, __started, &__result);
        __result
    }
}
