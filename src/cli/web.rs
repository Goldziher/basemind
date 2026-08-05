//! `basemind web` — the CLI half of the `web` domain.
//!
//! Real clap subcommands rather than a `--mode` flag, so each operation keeps its own `--help` and
//! its own argument validation; they map one-to-one onto the MCP `web` tool's [`WebMode`] values,
//! which is what `tests/cli_parity.rs` asserts.
//!
//! The MCP `web` tool only exists when the crate is built with `--features crawl`. The clap enum is
//! defined unconditionally so the subcommands always parse; dispatch returns a clear "built without
//! crawl feature" error when the feature is off (mirroring MCP's behavior of not exposing the tool
//! at all).

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

    /// Every field the `web` tool accepts, with the ones this mode does not use left `None` — the
    /// helper rejects a field that belongs to another mode, so they must not be populated blindly.
    fn params(mode: WebMode, url: crate::url::Url) -> WebParams {
        WebParams {
            mode,
            url,
            index: None,
            scope: None,
            max_pages: None,
            max_depth: None,
            limit: None,
        }
    }

    let p = match cmd {
        WebCmd::Scrape { url, no_index, scope } => WebParams {
            index: no_index.then_some(false),
            scope,
            ..params(WebMode::Scrape, parse_url(&url)?)
        },
        WebCmd::Crawl {
            url,
            max_pages,
            max_depth,
            scope,
        } => WebParams {
            max_pages,
            max_depth,
            scope,
            ..params(WebMode::Crawl, parse_url(&url)?)
        },
        WebCmd::Map { url, limit } => WebParams {
            limit,
            ..params(WebMode::Map, parse_url(&url)?)
        },
    };

    let key = p.mode.telemetry_key();
    let r = run_tool(key, server.web(Parameters(Lenient(p))).await)?;
    emit(key, &r, opts, out)
}

#[cfg(not(feature = "crawl"))]
pub async fn run(_server: &BasemindServer, _cmd: WebCmd, _opts: &Emit, _out: &mut impl Write) -> Result<()> {
    anyhow::bail!("this `basemind` was built without the `crawl` feature; rebuild with --features crawl")
}
