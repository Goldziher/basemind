//! Build the shared `crawlberg` engine handle from a `CrawlConfig`.
//!
//! The engine holds the reqwest client, robots.txt cache, and (when configured)
//! dispatch policy. It is cheap to clone (`Arc`-backed) and is created once
//! per `BasemindServer` boot.

use std::time::Duration;

use anyhow::{Context, Result};
use crawlberg::{CrawlConfig as KcCrawlConfig, CrawlEngineHandle, SsrfPolicy, create_engine};

use crate::config::CrawlConfig;

/// Hard cap on the URLs a single `web_map` fetch materializes. crawlberg 1.0.6 threads `map_limit`
/// through the sitemap fetch loop and stops fetching + parsing once it is reached, bounding peak
/// memory to roughly this cap plus one child sitemap (crawlberg#33). Without it a large sitemap-index
/// host — docs.rs alone is ~482k URLs — drove serve past 5 GB and was OOM-killed. It is also the
/// ceiling on a per-call `web_map` limit, so no request can ask for more than one bounded fetch yields.
pub const WEB_MAP_URL_CAP: u32 = 1000;

/// Translate basemind's `CrawlConfig` into crawlberg's runtime `CrawlConfig`.
///
/// Pure and total (modulo the `usize` range checks) so the mapping — including
/// the on-host scope invariant — is unit-testable without standing up an engine.
fn kc_config(cfg: &CrawlConfig) -> Result<KcCrawlConfig> {
    let max_pages = usize::try_from(cfg.max_pages).context("max_pages exceeds usize")?;
    let max_depth = usize::try_from(cfg.max_depth).context("max_depth exceeds usize")?;
    let max_body_size = usize::try_from(cfg.max_body_size).context("max_body_size exceeds usize")?;

    Ok(KcCrawlConfig {
        max_pages: Some(max_pages),
        max_depth: Some(max_depth),
        respect_robots_txt: cfg.respect_robots_txt,
        user_agent: Some(cfg.user_agent.clone()),
        max_body_size: Some(max_body_size),
        max_concurrent: Some(4),
        request_timeout: Duration::from_secs(30),
        stay_on_domain: true,
        map_limit: Some(WEB_MAP_URL_CAP as usize),
        ssrf: SsrfPolicy {
            deny_private: !cfg.allow_private_network,
            ..Default::default()
        },
        ..Default::default()
    })
}

/// Translate basemind's `CrawlConfig` into crawlberg's runtime config and
/// instantiate the engine. Returns an error when the user-supplied user-agent
/// is empty or crawlberg rejects the validated config — both indicate a
/// configuration bug rather than a transient network issue.
pub fn build_engine(cfg: &CrawlConfig) -> Result<CrawlEngineHandle> {
    if !cfg.respect_robots_txt {
        tracing::warn!("crawl.respect_robots_txt is disabled — basemind will fetch URLs that robots.txt forbids");
    }

    create_engine(Some(kc_config(cfg)?)).context("create crawlberg engine")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kc_config_stays_on_the_seed_host() {
        let kc = kc_config(&CrawlConfig::default()).expect("default crawl config maps cleanly");
        assert!(
            kc.stay_on_domain,
            "web_crawl must stay on the seed host — a crawl that follows off-host links is bug #34"
        );
        assert!(
            !kc.allow_subdomains,
            "same-host scope excludes subdomains unless a caller widens it explicitly"
        );
        assert_eq!(
            kc.map_limit,
            Some(WEB_MAP_URL_CAP as usize),
            "web_map must bound the sitemap fetch to cap peak memory (crawlberg#33)"
        );
    }
}
