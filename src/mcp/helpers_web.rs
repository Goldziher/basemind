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

use futures::StreamExt as _;
use futures::stream::FuturesUnordered;
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

/// POST-FETCH defence-in-depth SSRF check on a URL that kreuzcrawl actually
/// hit.
///
/// `Url::parse` enforces the private-host denylist on every *requested* URL, but
/// kreuzcrawl follows HTTP redirects itself, so a public seed can 30x-redirect
/// to a private target (`https://evil.com` → 302 → `http://169.254.169.254/`)
/// that the seed validation never saw. Here we re-validate the URL the crawler
/// landed on through the same denylist and refuse to index when it resolves to a
/// private / loopback / link-local host.
///
/// This does NOT prevent the redirect GET itself — the request to the private
/// host has already happened by the time we see `final_url` / `normalized_url`.
/// Fully blocking the redirect fetch would require a redirect-policy hook inside
/// kreuzcrawl's HTTP client, which is un-vendored here. This guard is the layer
/// we control: it stops private-host content from ever landing in the index.
fn reject_redirected_private_url(context: &str, fetched_url: &str) -> Result<(), McpError> {
    match crate::url::Url::parse(fetched_url) {
        Ok(_) => Ok(()),
        Err(crate::url::UrlError::PrivateHost(host)) => Err(McpError::invalid_params(
            format!(
                "{context}: refusing to index private/loopback host reached via redirect: {host} \
                 (set BASEMIND_ALLOW_PRIVATE_HOSTS=1 to allow)"
            ),
            None,
        )),
        // A non-denylist parse failure (e.g. an exotic scheme the crawler
        // normalised to) is also unsafe to index — fail closed rather than open.
        Err(other) => Err(McpError::invalid_params(
            format!("{context}: refusing to index unparsable fetched URL {fetched_url:?}: {other}"),
            None,
        )),
    }
}

/// Resolve the LanceDB scope tag for a fetched page.
///
/// When the caller supplied an explicit `scope`, honour it verbatim. Otherwise
/// derive the scope from the page's *final* URL (after redirects) rather than
/// the requested URL, so a redirect across hosts (e.g. `example.com` →
/// `cdn.example.net`) lands the rows under the host they actually came from and
/// `search_documents { scope: "web:<host>" }` retrieves them. Falls back to the
/// requested URL's scope if the final URL fails to parse (it should not for an
/// http/https response, but we never panic on a server-supplied string).
fn resolve_scope(explicit: Option<&str>, requested: &crate::url::Url, final_url: &str) -> String {
    if let Some(scope) = explicit {
        return scope.to_string();
    }
    match crate::url::Url::parse(final_url) {
        Ok(resolved) => default_scope(&resolved),
        Err(_) => default_scope(requested),
    }
}

/// Reject a `Some(0)` crawl override, mirroring the schema's `min = 1`. `None`
/// (inherit server default) and `Some(n >= 1)` pass through.
#[cfg(feature = "crawl")]
fn reject_zero_override(field: &str, value: Option<u32>) -> Result<(), McpError> {
    if value == Some(0) {
        return Err(McpError::invalid_params(
            format!("{field} must be >= 1"),
            None,
        ));
    }
    Ok(())
}

/// Build a per-call kreuzcrawl engine that overrides `max_pages` / `max_depth`
/// for this request only, leaving the server's shared `[crawl]` defaults intact.
///
/// kreuzcrawl bakes the page/depth caps into the engine handle, so honouring a
/// per-call override means constructing a fresh engine from a cloned config.
/// `None` overrides fall back to the server default.
#[cfg(feature = "crawl")]
fn per_call_engine(
    state: &ServerState,
    max_pages: Option<u32>,
    max_depth: Option<u32>,
) -> Result<kreuzcrawl::CrawlEngineHandle, McpError> {
    let mut cfg = state.config.crawl.clone();
    if let Some(mp) = max_pages {
        cfg.max_pages = mp;
    }
    if let Some(md) = max_depth {
        cfg.max_depth = md;
    }
    crate::web::build_engine(&cfg).map_err(|e| mcp_internal("build per-call crawl engine", e))
}

async fn embedder(state: &ServerState) -> Result<Arc<SharedEmbedder>, McpError> {
    // Use the configured embedding preset, not a hardcoded one. The disk
    // scanner embeds with `documents.embedding_preset`; if serve loaded a
    // different model the LanceStore (dim, model) mismatch would wipe the table
    // on the next open, so serve and disk scans must agree on the preset.
    let preset = state.config.documents.embedding_preset.clone();
    let embedder = state
        .embedder
        .get_or_try_init(|| async {
            SharedEmbedder::load(&preset)
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

    let result = kreuzcrawl::scrape(engine, &url_str)
        .await
        .map_err(|e| mcp_internal("kreuzcrawl scrape", e))?;

    // POST-FETCH SSRF guard: kreuzcrawl may have followed a redirect from the
    // (validated) seed to a private host. Re-validate the URL we actually
    // landed on before indexing anything from it.
    reject_redirected_private_url("web_scrape", &result.final_url)?;

    // Derive the scope from the FINAL url (post-redirect), not the requested
    // host — the rows we store are keyed by `result.final_url`, so the scope
    // must match the host they actually came from.
    let scope = resolve_scope(params.scope.as_deref(), &params.url, &result.final_url);

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
    // When both overrides are None reuse the shared engine — no config clone,
    // no new engine construction. When either override is set, build a one-shot
    // engine from a cloned config (kreuzcrawl bakes caps into the engine handle).
    // Validate the shared engine is live either way so the error surface matches
    // the other web tools.
    engine(state)?;
    // Reject zero overrides at the boundary: the JSON schema declares `min = 1`
    // for both, but a hand-crafted MCP request can still send `0`, which would
    // bake a degenerate crawl (0 pages / 0 depth) into the per-call engine.
    // `None` keeps the server default.
    reject_zero_override("max_pages", params.max_pages)?;
    reject_zero_override("max_depth", params.max_depth)?;
    // `engine` borrow must outlive the crawl call; hold in a local so the
    // `Cow` borrow of the shared handle compiles without a use-after-free.
    let per_call_handle;
    let engine_ref = if params.max_pages.is_none() && params.max_depth.is_none() {
        // Zero-override fast path: borrow the already-initialized shared handle.
        engine(state)?
    } else {
        per_call_handle = per_call_engine(state, params.max_pages, params.max_depth)?;
        &per_call_handle
    };
    let url_str = params.url.as_str().to_string();

    let crawl_outcome = kreuzcrawl::crawl(engine_ref, &url_str)
        .await
        .map_err(|e| mcp_internal("kreuzcrawl crawl", e))?;

    // Top-level scope echoed in the response: explicit when supplied, else
    // derived from the seed URL's host. Per-page rows derive their own scope
    // from the page's final URL below (a crawl can span subdomains).
    let scope = params
        .scope
        .clone()
        .unwrap_or_else(|| default_scope(&params.url));

    // Maximum concurrent ONNX embed + LanceDB write tasks per `web_crawl` call.
    // Each task runs on a blocking thread. The semaphore caps active tasks so
    // the blocking pool doesn't exhaust under large crawls. `LanceStore` wraps
    // a current-thread tokio runtime; tokio's docs confirm that `block_on` on a
    // current-thread runtime IS safe to call concurrently from multiple threads —
    // the first caller "owns" the driver; other callers hook into it.
    const CRAWL_INDEX_CONCURRENCY: usize = 4;

    let pages_visited = crawl_outcome.pages.len();
    let lance = lance_store(state).await?;
    let embedder = embedder(state).await?;
    // Hoist the two chunking scalars out once rather than cloning the full
    // `DocumentsConfig` (which contains several `Vec<String>` + `BTreeMap`)
    // once per page closure.
    let documents_cfg = Arc::new(state.config.documents.clone());

    let mut total_chunks = 0usize;
    let mut pages_indexed = 0usize;
    // Track outcomes in insertion order. SSRF-rejected pages are written
    // directly as `Some`; indexing futures write their slot on completion.
    let mut outcomes: Vec<Option<WebCrawlPageOutcome>> =
        Vec::with_capacity(crawl_outcome.pages.len());
    let mut futs: FuturesUnordered<_> = FuturesUnordered::new();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(CRAWL_INDEX_CONCURRENCY));

    for (slot, page) in crawl_outcome.pages.into_iter().enumerate() {
        // POST-FETCH SSRF guard (defence-in-depth): a crawl can follow links /
        // redirects from a public seed onto a private host. Re-validate the URL
        // each page actually came from and skip indexing it when it resolves to
        // a private / loopback / link-local host. See
        // `reject_redirected_private_url` for why this can't block the GET
        // itself (kreuzcrawl owns the redirect policy, un-vendored here).
        if let Err(error) = reject_redirected_private_url("web_crawl", &page.normalized_url) {
            tracing::warn!(
                url = %page.normalized_url,
                "web_crawl: skipping private/loopback page reached via crawl"
            );
            outcomes.push(Some(WebCrawlPageOutcome {
                url: page.normalized_url,
                status_code: page.status_code,
                chunks_indexed: 0,
                indexed: false,
                error: Some(error.message.to_string()),
            }));
            continue;
        }

        let body_text = page
            .markdown
            .as_ref()
            .map(|m| m.content.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| page.html.clone());

        // Each page is stored under `page.normalized_url`; derive its scope from
        // that same URL's host so rows land under the host they came from, not
        // the seed host (a crawl can follow links across subdomains). An
        // explicit caller scope still wins for every page.
        let page_scope = match params.scope.as_deref() {
            Some(s) => s.to_string(),
            None => match crate::url::Url::parse(&page.normalized_url) {
                Ok(u) => default_scope(&u),
                Err(_) => scope.clone(),
            },
        };

        let lance_for_block = Arc::clone(&lance);
        let embedder_for_block = Arc::clone(&embedder);
        // Arc clone = one atomic increment per page (vs a full DocumentsConfig
        // deep clone). The async closure below accesses it with a shared borrow.
        let docs_for_block = Arc::clone(&documents_cfg);
        let sem = Arc::clone(&semaphore);
        let scope_for_block = page_scope;
        let path_for_block = page.normalized_url.clone();
        let mime_for_block = page.content_type.clone();
        let status_code = page.status_code;
        let normalized_url = page.normalized_url.clone();

        // Reserve a slot now; the future writes its `(slot, outcome)` pair.
        outcomes.push(None);

        futs.push(async move {
            // Acquire the semaphore before entering the blocking pool so at
            // most CRAWL_INDEX_CONCURRENCY tasks are active simultaneously.
            let _permit = sem.acquire().await;
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
                Ok(Ok(indexed)) => WebCrawlPageOutcome {
                    url: normalized_url,
                    status_code,
                    chunks_indexed: indexed.chunks_indexed,
                    indexed: indexed.chunks_indexed > 0,
                    error: None,
                },
                Ok(Err(error)) => {
                    tracing::warn!(url = %normalized_url, ?error, "web_crawl index_page failed");
                    WebCrawlPageOutcome {
                        url: normalized_url,
                        status_code,
                        chunks_indexed: 0,
                        indexed: false,
                        error: Some(error.to_string()),
                    }
                }
                Err(join_err) => WebCrawlPageOutcome {
                    url: normalized_url,
                    status_code,
                    chunks_indexed: 0,
                    indexed: false,
                    error: Some(format!("spawn_blocking: {join_err}")),
                },
            };
            (slot, outcome)
        });
    }

    // Drive all concurrent indexing futures to completion, placing results back
    // into assigned slots to preserve caller-visible insertion order.
    while let Some((slot, outcome)) = futs.next().await {
        if outcome.indexed {
            pages_indexed += 1;
            total_chunks += outcome.chunks_indexed;
        }
        outcomes[slot] = Some(outcome);
    }

    // Unwrap sentinels — every slot is now `Some`: SSRF-rejected slots were
    // written directly above; indexing slots were written in the drive loop.
    let outcomes: Vec<WebCrawlPageOutcome> = outcomes.into_iter().flatten().collect();

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

#[cfg(test)]
mod tests {
    use super::*;

    // `reject_redirected_private_url` consults the same process-global
    // `BASEMIND_ALLOW_PRIVATE_HOSTS` env as `Url::parse`; serialize the env
    // mutation on the CRATE-WIDE lock shared with the `url` and `web::ingest`
    // test modules so a setter in one module never observes a remover here.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::url::PRIVATE_HOSTS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn rejects_zero_max_pages_and_depth() {
        assert!(reject_zero_override("max_pages", Some(0)).is_err());
        assert!(reject_zero_override("max_depth", Some(0)).is_err());
        // None (inherit server default) and >= 1 pass through.
        assert!(reject_zero_override("max_pages", None).is_ok());
        assert!(reject_zero_override("max_pages", Some(1)).is_ok());
        assert!(reject_zero_override("max_depth", Some(50)).is_ok());
    }

    #[test]
    fn zero_override_error_names_the_field_and_bound() {
        let err = reject_zero_override("max_pages", Some(0)).expect_err("0 must reject");
        assert!(
            err.message.contains("max_pages") && err.message.contains(">= 1"),
            "error should name the field and the >= 1 bound; got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_private_redirect_target() {
        let _g = env_lock();
        unsafe { std::env::remove_var("BASEMIND_ALLOW_PRIVATE_HOSTS") };
        // Simulates the URL kreuzcrawl landed on AFTER following a redirect from a
        // public seed to the AWS metadata endpoint — the canonical SSRF target.
        let err =
            reject_redirected_private_url("web_scrape", "http://169.254.169.254/latest/meta-data/")
                .expect_err("link-local redirect target must be rejected");
        assert!(
            err.message.contains("169.254.169.254"),
            "rejection should name the private host; got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_loopback_redirect_target() {
        let _g = env_lock();
        unsafe { std::env::remove_var("BASEMIND_ALLOW_PRIVATE_HOSTS") };
        assert!(reject_redirected_private_url("web_crawl", "http://127.0.0.1:9000/").is_err());
        assert!(reject_redirected_private_url("web_crawl", "http://localhost/admin").is_err());
    }

    #[test]
    fn allows_public_redirect_target() {
        let _g = env_lock();
        unsafe { std::env::remove_var("BASEMIND_ALLOW_PRIVATE_HOSTS") };
        assert!(reject_redirected_private_url("web_scrape", "https://example.com/landing").is_ok());
    }
}
