//! Build the shared `crawlberg` engine handle from a `CrawlConfig`.
//!
//! The engine holds the reqwest client, robots.txt cache, and (when configured)
//! dispatch policy. It is cheap to clone (`Arc`-backed) and is created once
//! per `BasemindServer` boot.

use std::time::Duration;

use anyhow::{Context, Result};
use crawlberg::{CrawlConfig as KcCrawlConfig, CrawlEngineHandle, SsrfPolicy, create_engine};

use crate::config::CrawlConfig;

/// Translate basemind's `CrawlConfig` into crawlberg's runtime config and
/// instantiate the engine. Returns an error when the user-supplied user-agent
/// is empty or crawlberg rejects the validated config — both indicate a
/// configuration bug rather than a transient network issue.
pub fn build_engine(cfg: &CrawlConfig) -> Result<CrawlEngineHandle> {
    let max_pages = usize::try_from(cfg.max_pages).context("max_pages exceeds usize")?;
    let max_depth = usize::try_from(cfg.max_depth).context("max_depth exceeds usize")?;
    let max_body_size = usize::try_from(cfg.max_body_size).context("max_body_size exceeds usize")?;

    if !cfg.respect_robots_txt {
        tracing::warn!("crawl.respect_robots_txt is disabled — basemind will fetch URLs that robots.txt forbids");
    }

    let kc_cfg = KcCrawlConfig {
        max_pages: Some(max_pages),
        max_depth: Some(max_depth),
        respect_robots_txt: cfg.respect_robots_txt,
        user_agent: Some(cfg.user_agent.clone()),
        max_body_size: Some(max_body_size),
        max_concurrent: Some(4),
        request_timeout: Duration::from_secs(30),
        ssrf: SsrfPolicy {
            deny_private: !cfg.allow_private_network,
            ..Default::default()
        },
        ..Default::default()
    };

    create_engine(Some(kc_cfg)).context("create crawlberg engine")
}
