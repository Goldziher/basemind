//! In-process MCP serve entry points over an in-memory duplex transport, for integration tests.
//!
//! Since the stdio server transport was removed (the daemon's streamable-HTTP front-end is the only
//! production transport), integration tests that used to spawn `basemind serve` as a child process
//! and speak MCP over its stdio pipes now drive a server hosted IN-PROCESS over an
//! [`tokio::io::duplex`] pair. Each helper mirrors an assembly the old in-process serve did, builds a
//! full [`BasemindServer`], and pumps it over one end of the duplex — the caller serves an rmcp
//! client over the other end.
//!
//! Gated behind `feature = "test-support"` so it never ships in a production build.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use super::{BasemindServer, ServerOptions};
use crate::store::{LockHolder, Store};

/// Duplex buffer size: generous enough that a single JSON-RPC frame never blocks the pump before the
/// codec drains it. 1 MiB comfortably holds a `tools/list` payload.
const DUPLEX_BUFFER: usize = 1 << 20;

/// Per-category git-cache LRU capacity for the in-memory test server. Matches the `basemind serve`
/// default; the on-disk cache is disabled so nothing persists between test runs.
const GIT_CACHE_MEM: usize = 1024;

/// Serve a fully-built [`BasemindServer`] over an in-memory duplex, returning the CLIENT end. The
/// server task is detached onto the current tokio runtime and lives until the client end is dropped
/// (which closes the pipe and ends the pump). The caller serves an rmcp client over the returned
/// stream, e.g. `().serve(transport).await`.
fn spawn_over_duplex(server: BasemindServer) -> tokio::io::DuplexStream {
    use rmcp::ServiceExt;

    let (client_end, server_end) = tokio::io::duplex(DUPLEX_BUFFER);
    tokio::spawn(async move {
        match server.serve(server_end).await {
            Ok(running) => {
                if let Err(error) = running.waiting().await {
                    tracing::debug!(%error, "in-memory serve: session ended with error");
                }
            }
            Err(error) => tracing::debug!(%error, "in-memory serve: handshake failed"),
        }
    });
    client_end
}

/// The config / repo / git-cache bits every in-memory server construction needs.
type SupportBits = (
    Arc<crate::config::Config>,
    Option<Arc<crate::git::Repo>>,
    Arc<crate::git_cache::GitCache>,
);

/// Build the [`SupportBits`] every in-memory server needs, from a repo `root`.
fn build_support_bits(root: &Path) -> Result<SupportBits> {
    let basemind_dir = crate::store::workspace_cache_dir(root);
    let config = Arc::new(crate::config::load_or_default_logged(root));
    let repo = crate::git::Repo::discover(root).ok().map(Arc::new);
    let git_cache =
        Arc::new(crate::git_cache::GitCache::open(&basemind_dir, GIT_CACHE_MEM, false).context("open git cache")?);
    Ok((config, repo, git_cache))
}

/// Open the `view` store read-write in-process, build a full [`BasemindServer`] (background
/// facilities on, exactly like a non-comms `basemind serve`), and serve it over an in-memory duplex
/// transport. Returns the CLIENT end of the duplex.
///
/// Intended for integration tests only — gated behind `feature = "test-support"`.
pub async fn serve_in_memory(root: &Path, view: &str) -> Result<tokio::io::DuplexStream> {
    serve_in_memory_configured(root, view, None).await
}

/// Like [`serve_in_memory`], but an offload-eligible call becomes a SEP-2663 task after
/// `threshold` instead of the production [`TASK_THRESHOLD`](super::tasks::TASK_THRESHOLD), so a
/// test can exercise the task path (a zero threshold) without a genuinely slow call.
pub async fn serve_in_memory_with_task_threshold(
    root: &Path,
    view: &str,
    threshold: std::time::Duration,
) -> Result<tokio::io::DuplexStream> {
    serve_in_memory_configured(root, view, Some(threshold)).await
}

async fn serve_in_memory_configured(
    root: &Path,
    view: &str,
    task_threshold: Option<std::time::Duration>,
) -> Result<tokio::io::DuplexStream> {
    crate::cli::env_config::ensure_env_data_home_installed().map_err(anyhow::Error::msg)?;
    // Mirror the old in-process `serve` fallback: hold the write lock when free, else open read-only
    // so a SECOND in-memory server on the same repo (e.g. a lean vs full comparison in one test
    // process) still reads the shared index instead of hard-failing on lock contention.
    let (store, read_only) = match Store::open_with_holder(root, view, LockHolder::Serve) {
        Ok(store) => (store, false),
        Err(error) if error.is_lock_contention() => (Store::open_read_only(root, view)?, true),
        Err(error) => return Err(anyhow::Error::new(error).context("open store")),
    };
    let (config, repo, git_cache) = build_support_bits(root)?;
    let options = ServerOptions {
        read_only,
        ..ServerOptions::default()
    };
    let server = BasemindServer::new_with_options(store, root.to_path_buf(), config, repo, git_cache, options);
    if let Some(threshold) = task_threshold {
        server.state.task_threshold_ms.store(
            u64::try_from(threshold.as_millis()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    Ok(spawn_over_duplex(server))
}
