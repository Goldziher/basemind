//! In-process server construction for the CLI.
//!
//! Builds a [`BasemindServer`] with every background facility disabled
//! ([`BasemindServer::new_oneshot`]) so a single CLI tool call runs the identical
//! code path an MCP client would, then the process exits. The store is opened
//! read-only so the CLI never contends for the `.basemind/.lock` flock a running
//! `basemind serve` may already hold.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config::{self, Config, DocumentsCliOverrides};
use crate::git::Repo;
use crate::git_cache::GitCache;
use crate::mcp::BasemindServer;
use crate::store::Store;

/// LRU capacity per git-cache category for one-shot CLI invocations. Small —
/// a CLI process issues a single query and exits, so a big LRU never pays off.
const CLI_GIT_CACHE_MEM: usize = 256;

/// Construct a one-shot [`BasemindServer`] for the given repo `root` and `view`.
///
/// Mirrors the construction `cmd_serve` performs (config load, repo discover,
/// git cache open) but opens the store read-only and disables all background
/// facilities. `documents` flows the `#[command(flatten)]` document overrides
/// into the resolved config the same way `serve` does.
pub fn build_server(
    root: &Path,
    view: &str,
    documents: DocumentsCliOverrides,
) -> Result<BasemindServer> {
    let store = Store::open_read_only(root, view).context("open store (read-only)")?;
    let basemind_dir = root.join(config::BASEMIND_DIR);
    let cfg = Arc::new(load_config(root, documents)?);
    let repo = Repo::discover(root).ok().map(Arc::new);
    // Disk cache off for the CLI: a one-shot process never benefits from the
    // persisted git cache and disabling it avoids writing under a path a live
    // server might own.
    let git_cache = Arc::new(
        GitCache::open(&basemind_dir, CLI_GIT_CACHE_MEM, false).context("open git cache")?,
    );
    Ok(BasemindServer::new_oneshot(
        store,
        root.to_path_buf(),
        cfg,
        repo,
        git_cache,
    ))
}

/// Load the resolved config, applying the document CLI override layer. Falls back
/// to defaults when no `.basemind/basemind.toml` exists.
fn load_config(root: &Path, documents: DocumentsCliOverrides) -> Result<Config> {
    match config::load_with_overrides(root, None, Some(documents)) {
        Ok(loaded) => Ok(loaded.config),
        Err(config::ConfigError::NotFound(_)) => Ok(config::default_for_root(root)),
        Err(e) => Err(anyhow::anyhow!(e)),
    }
}
