//! Web ingestion subcommands.
//!
//! The MCP `web_*` methods only exist when the crate is built with
//! `--features crawl`. The clap enum is defined unconditionally so the
//! subcommands always parse; dispatch returns a clear "built without crawl
//! feature" error when the feature is off (mirroring MCP's behavior of not
//! exposing the tools at all).

use std::io::Write;

use anyhow::Result;
use clap::Subcommand;

use super::render::Emit;
use crate::mcp::BasemindServer;

#[derive(Subcommand, Debug)]
pub enum WebCmd {
    /// Fetch a single URL, extract + embed it into the documents store.
    Scrape {
        url: String,
        /// Fetch metadata only; do not embed/index.
        #[arg(long)]
        no_index: bool,
        #[arg(long)]
        scope: Option<String>,
    },
    /// Crawl a website starting from a seed URL.
    Crawl {
        url: String,
        #[arg(long)]
        max_pages: Option<u32>,
        #[arg(long)]
        max_depth: Option<u32>,
        #[arg(long)]
        scope: Option<String>,
    },
    /// Discover URLs on a site via sitemap + link map (no body fetch).
    Map {
        url: String,
        /// Cap the URLs returned. Default 100, max 1000. `total_urls` still reports the full count.
        #[arg(long)]
        limit: Option<u32>,
    },
}

#[cfg(feature = "crawl")]
pub async fn run(server: &BasemindServer, cmd: WebCmd, opts: &Emit, out: &mut impl Write) -> Result<()> {
    use crate::mcp::params::*;

    use super::render::emit;
    use super::run_tool;

    fn parse_url(s: &str) -> Result<crate::url::Url> {
        s.parse::<crate::url::Url>()
            .map_err(|e| anyhow::anyhow!("invalid url {s:?}: {e}"))
    }

    match cmd {
        WebCmd::Scrape { url, no_index, scope } => {
            let p = WebScrapeParams {
                url: parse_url(&url)?,
                index: !no_index,
                scope,
            };
            let r = run_tool("web_scrape", server.web_scrape(Parameters(p)).await)?;
            emit("web_scrape", &r, opts, out)
        }
        WebCmd::Crawl {
            url,
            max_pages,
            max_depth,
            scope,
        } => {
            let p = WebCrawlParams {
                url: parse_url(&url)?,
                max_pages,
                max_depth,
                scope,
            };
            let r = run_tool("web_crawl", server.web_crawl(Parameters(p)).await)?;
            emit("web_crawl", &r, opts, out)
        }
        WebCmd::Map { url, limit } => {
            let p = WebMapParams {
                url: parse_url(&url)?,
                limit,
            };
            let r = run_tool("web_map", server.web_map(Parameters(p)).await)?;
            emit("web_map", &r, opts, out)
        }
    }
}

#[cfg(not(feature = "crawl"))]
pub async fn run(_server: &BasemindServer, _cmd: WebCmd, _opts: &Emit, _out: &mut impl Write) -> Result<()> {
    anyhow::bail!("this `basemind` was built without the `crawl` feature; rebuild with --features crawl")
}
