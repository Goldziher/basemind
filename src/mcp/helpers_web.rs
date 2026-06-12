//! Helper bodies for the three web ingestion MCP tools.
//!
//! Each helper:
//!  1. resolves the shared crawl engine + embedder + LanceDB store from
//!     `ServerState` (returning an MCP error when the feature was compiled in
//!     but the engine failed to initialize),
//!  2. runs the kreuzcrawl operation on the request URL,
//!  3. routes resulting page bodies through [`crate::web::ingest::index_page`]
//!     to land them in the existing `documents` LanceDB table.
//!
//! The whole module is gated on `feature = "crawl"` — when the feature is off
//! the file does not compile at all, and the corresponding tool router is not
//! registered on `BasemindServer`.

#![cfg(feature = "crawl")]

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers::json_result;
use super::memory::lance_store;
use super::types::{
    WebCrawlPageOutcome, WebCrawlParams, WebCrawlResponse, WebMapEntry, WebMapParams,
    WebMapResponse, WebScrapeParams, WebScrapeResponse,
};
use crate::embeddings::SharedEmbedder;
use crate::web::ingest::{default_scope, index_page};

fn mcp_internal(prefix: &str, err: impl std::fmt::Display) -> McpError {
    McpError::internal_error(format!("{prefix}: {err}"), None)
}

async fn embedder(state: &ServerState) -> Result<Arc<SharedEmbedder>, McpError> {
    let embedder = state
        .embedder
        .get_or_try_init(|| async {
            SharedEmbedder::load("balanced")
                .map(Arc::new)
                .map_err(|e| format!("load embedder: {e}"))
        })
        .await
        .map_err(|e| McpError::internal_error(e.clone(), None))?;
    Ok(Arc::clone(embedder))
}

fn engine(state: &ServerState) -> Result<&kreuzcrawl::CrawlEngineHandle, McpError> {
    state.crawl_engine.as_ref().ok_or_else(|| {
        McpError::internal_error(
            "crawl engine not initialised; check basemind serve startup logs",
            None,
        )
    })
}

pub(super) async fn run_web_scrape(
    state: &ServerState,
    params: WebScrapeParams,
) -> Result<CallToolResult, McpError> {
    let engine = engine(state)?;
    let url_str = params.url.as_str().to_string();
    let scope = params.scope.unwrap_or_else(|| default_scope(&params.url));

    let result = kreuzcrawl::scrape(engine, &url_str)
        .await
        .map_err(|e| mcp_internal("kreuzcrawl scrape", e))?;

    let body_text: String = result
        .markdown
        .as_ref()
        .map(|m| m.content.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| result.html.clone());

    let response = if params.index {
        let lance = lance_store(state).await?;
        let embedder = embedder(state).await?;
        let documents_cfg = state.config.documents.clone();
        let scope_for_block = scope.clone();
        let final_url_for_block = result.final_url.clone();
        let mime_for_block = result.content_type.clone();
        let indexed = tokio::task::spawn_blocking(move || {
            index_page(
                lance.as_ref(),
                &embedder,
                &documents_cfg,
                &scope_for_block,
                &final_url_for_block,
                &mime_for_block,
                &body_text,
            )
        })
        .await
        .map_err(|e| mcp_internal("spawn_blocking", e))?
        .map_err(|e| mcp_internal("index_page", e))?;

        WebScrapeResponse {
            url: url_str,
            final_url: result.final_url,
            status_code: result.status_code,
            content_type: result.content_type,
            bytes: indexed.bytes,
            chunks_indexed: indexed.chunks_indexed,
            indexed: indexed.chunks_indexed > 0,
            scope,
        }
    } else {
        WebScrapeResponse {
            url: url_str,
            final_url: result.final_url,
            status_code: result.status_code,
            content_type: result.content_type,
            bytes: body_text.len(),
            chunks_indexed: 0,
            indexed: false,
            scope,
        }
    };

    json_result(&response)
}

pub(super) async fn run_web_crawl(
    state: &ServerState,
    params: WebCrawlParams,
) -> Result<CallToolResult, McpError> {
    // kreuzcrawl's per-call config knobs (max_pages, max_depth) live on
    // CrawlConfig, which is owned by the shared engine. Cloning the engine to
    // apply per-call overrides is not supported by the public API today —
    // honour the request shape but emit a warn when overrides differ from the
    // server defaults, so an agent at least sees the discrepancy in logs.
    let cfg = &state.config.crawl;
    if let Some(mp) = params.max_pages
        && mp != cfg.max_pages
    {
        tracing::warn!(
            requested = mp,
            server_default = cfg.max_pages,
            "web_crawl.max_pages override ignored — using server default"
        );
    }
    if let Some(md) = params.max_depth
        && md != cfg.max_depth
    {
        tracing::warn!(
            requested = md,
            server_default = cfg.max_depth,
            "web_crawl.max_depth override ignored — using server default"
        );
    }

    let engine = engine(state)?;
    let url_str = params.url.as_str().to_string();
    let scope = params.scope.unwrap_or_else(|| default_scope(&params.url));

    let crawl_outcome = kreuzcrawl::crawl(engine, &url_str)
        .await
        .map_err(|e| mcp_internal("kreuzcrawl crawl", e))?;

    let pages_visited = crawl_outcome.pages.len();
    let lance = lance_store(state).await?;
    let embedder = embedder(state).await?;
    let documents_cfg = state.config.documents.clone();

    let mut total_chunks = 0usize;
    let mut pages_indexed = 0usize;
    let mut outcomes: Vec<WebCrawlPageOutcome> = Vec::with_capacity(crawl_outcome.pages.len());

    for page in crawl_outcome.pages {
        let body_text = page
            .markdown
            .as_ref()
            .map(|m| m.content.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| page.html.clone());

        let lance_for_block = Arc::clone(&lance);
        let embedder_for_block = Arc::clone(&embedder);
        let docs_for_block = documents_cfg.clone();
        let scope_for_block = scope.clone();
        let path_for_block = page.normalized_url.clone();
        let mime_for_block = page.content_type.clone();

        let res = tokio::task::spawn_blocking(move || {
            index_page(
                lance_for_block.as_ref(),
                &embedder_for_block,
                &docs_for_block,
                &scope_for_block,
                &path_for_block,
                &mime_for_block,
                &body_text,
            )
        })
        .await;

        let outcome = match res {
            Ok(Ok(indexed)) => {
                if indexed.chunks_indexed > 0 {
                    pages_indexed += 1;
                    total_chunks += indexed.chunks_indexed;
                }
                WebCrawlPageOutcome {
                    url: page.normalized_url,
                    status_code: page.status_code,
                    chunks_indexed: indexed.chunks_indexed,
                    indexed: indexed.chunks_indexed > 0,
                    error: None,
                }
            }
            Ok(Err(error)) => {
                tracing::warn!(url = %page.normalized_url, ?error, "web_crawl index_page failed");
                WebCrawlPageOutcome {
                    url: page.normalized_url,
                    status_code: page.status_code,
                    chunks_indexed: 0,
                    indexed: false,
                    error: Some(error.to_string()),
                }
            }
            Err(join_err) => WebCrawlPageOutcome {
                url: page.normalized_url,
                status_code: page.status_code,
                chunks_indexed: 0,
                indexed: false,
                error: Some(format!("spawn_blocking: {join_err}")),
            },
        };
        outcomes.push(outcome);
    }

    json_result(&WebCrawlResponse {
        seed_url: url_str,
        pages_visited,
        pages_indexed,
        total_chunks,
        scope,
        pages: outcomes,
        error: crawl_outcome.error,
    })
}

pub(super) async fn run_web_map(
    state: &ServerState,
    params: WebMapParams,
) -> Result<CallToolResult, McpError> {
    let engine = engine(state)?;
    let url_str = params.url.as_str().to_string();

    let map = kreuzcrawl::map_urls(engine, &url_str)
        .await
        .map_err(|e| mcp_internal("kreuzcrawl map_urls", e))?;

    let urls: Vec<WebMapEntry> = map
        .urls
        .into_iter()
        .map(|u| WebMapEntry {
            url: u.url,
            lastmod: u.lastmod,
            changefreq: u.changefreq,
            priority: u.priority,
        })
        .collect();

    json_result(&WebMapResponse {
        url: url_str,
        total_urls: urls.len(),
        urls,
    })
}
