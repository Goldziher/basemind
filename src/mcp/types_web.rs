//! Web scrape / crawl / map request + response shapes for the `crawl` feature.
//! Extracted from `types.rs` to keep that file within the per-file size budget.
//!
//! [`WebParams`] is what crosses the wire: one flat parameter object for the single `web` tool,
//! with a required [`WebMode`] selecting the operation and every per-mode field an optional
//! sibling. The per-operation structs below stay as the helpers' internal shapes.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::mode::WebMode;

/// Wire parameters for the `web` tool.
///
/// `url` is required by every mode; the rest apply to one mode each and are rejected — not ignored —
/// when passed to a mode that has no use for them (see [`super::mode::reject_unsupported`]).
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WebParams {
    /// Which operation to run.
    pub mode: WebMode,
    /// Absolute http or https URL: the page to fetch (`scrape`), the crawl seed (`crawl`), or the
    /// site to enumerate (`map`).
    pub url: crate::url::Url,
    /// `scrape` only. When true (the default), chunk + embed + write to LanceDB so the page is
    /// reachable via `memory` mode `documents`. When false, fetch and return metadata only —
    /// useful for previewing a URL before paying the embedding cost.
    #[serde(default)]
    pub index: Option<bool>,
    /// `scrape` and `crawl` only. LanceDB `scope` tag; defaults to `"web:<host>"`. Override to
    /// share a scope across many hosts or to namespace per project.
    #[serde(default)]
    pub scope: Option<String>,
    /// `crawl` only. Overrides the global `[crawl].max_pages` cap for this call.
    #[serde(default)]
    pub max_pages: Option<u32>,
    /// `crawl` only. Overrides the global `[crawl].max_depth` cap for this call.
    #[serde(default)]
    pub max_depth: Option<u32>,
    /// `map` only. Cap the number of URLs returned. Default 100, max 1000 — the crawlberg fetch cap
    /// that bounds peak memory. `total_urls` + `truncated` report whether more exist.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WebScrapeParams {
    /// Absolute http or https URL to fetch.
    pub url: crate::url::Url,
    /// When true (default), chunk + embed + write to LanceDB so the page is
    /// reachable via `memory` mode `documents`. When false, fetch and return
    /// metadata only — useful for previewing a URL before paying the embedding cost.
    #[serde(default = "WebScrapeParams::default_index")]
    pub index: bool,
    /// LanceDB `scope` tag. Default `"web:<host>"`. Override to share a scope
    /// across many hosts or to namespace per project.
    #[serde(default)]
    pub scope: Option<String>,
}

impl WebScrapeParams {
    fn default_index() -> bool {
        true
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct WebScrapeResponse {
    pub url: String,
    pub final_url: String,
    pub status_code: u16,
    pub content_type: String,
    pub bytes: usize,
    pub chunks_indexed: usize,
    pub indexed: bool,
    pub scope: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WebCrawlParams {
    /// Seed URL. The crawler follows links breadth-first from this page.
    pub url: crate::url::Url,
    /// Overrides the global `[crawl].max_pages` cap for this call only.
    #[serde(default)]
    pub max_pages: Option<u32>,
    /// Overrides the global `[crawl].max_depth` cap for this call only.
    #[serde(default)]
    pub max_depth: Option<u32>,
    /// LanceDB `scope` tag. Default `"web:<host>"` derived from the seed URL's
    /// host. Every page indexed by this crawl uses the same scope so
    /// `memory { mode: "documents", scope: ... }` retrieves them together.
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct WebCrawlResponse {
    pub seed_url: String,
    pub pages_visited: usize,
    pub pages_indexed: usize,
    pub total_chunks: usize,
    pub scope: String,
    /// Per-page indexing outcomes — surfaced so an agent can tell which URLs
    /// landed in LanceDB vs which were skipped (binary content, empty body).
    pub pages: Vec<WebCrawlPageOutcome>,
    /// Crawl-level error, if any (e.g. seed URL unreachable). Per-page errors
    /// land in `pages[*].error` instead.
    pub error: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct WebCrawlPageOutcome {
    pub url: String,
    pub status_code: u16,
    pub chunks_indexed: usize,
    pub indexed: bool,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WebMapParams {
    /// Site to discover. Returns sitemap entries + linked URLs without
    /// fetching their bodies.
    pub url: crate::url::Url,
    /// Cap the number of URLs returned. Default 100, max 1000 — the crawlberg fetch cap that bounds
    /// peak memory, so the fetch itself never materializes more. `total_urls` + `truncated` report
    /// whether more exist, so a capped response is never mistakable for a complete one.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct WebMapResponse {
    pub url: String,
    /// URLs discovered, up to the crawlberg fetch cap that bounds peak memory (crawlberg#33). A
    /// floor, not the site's true total: a host with more URLs than the cap reports exactly the cap
    /// and sets `truncated`. Below the cap it is exact.
    pub total_urls: usize,
    /// The caller is holding a page, not the whole site — either the per-call `limit` dropped
    /// entries, or the fetch hit the memory-safety cap and there may be more.
    pub truncated: bool,
    pub urls: Vec<WebMapEntry>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(super) struct WebMapEntry {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lastmod: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changefreq: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
}
