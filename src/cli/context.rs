//! In-process server construction for the CLI.
//!
//! Builds a [`BasemindServer`] with every background facility disabled
//! ([`BasemindServer::new_oneshot`]) so a single CLI tool call runs the identical
//! code path an MCP client would, then the process exits. The store is opened
//! read-only so the CLI never contends for the `.basemind/.lock` flock a running
//! `basemind serve` may already hold.
//!
//! When the comms daemon is up it is the sole index writer and holds the fjall lock, so the server
//! is built in the same `daemon_writer` shape the daemon's own `serve` relay uses: blobs-only
//! store, and every index read (references, callers, grep prefilter), memory op and rescan is
//! forwarded to the daemon rather than degrading to a truncating in-RAM projection or failing with
//! "index not available".

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config::{self, Config, DocumentsCliOverrides};
use crate::git::Repo;
use crate::git_cache::GitCache;
use crate::mcp::BasemindServer;
use crate::store::{LockHolder, Store};

use super::exit::CliExit;

/// What the caller intends to do with the server, which decides how the store is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Queries only: a read-only store.
    Read,
    /// A scan: the store is opened under the workspace writer lock (unless the daemon will do the
    /// writing), so an in-process rescan can never race another writer.
    Rescan,
}

/// LRU capacity per git-cache category for one-shot CLI invocations. Small —
/// a CLI process issues a single query and exits, so a big LRU never pays off.
const CLI_GIT_CACHE_MEM: usize = 256;

/// Construct a one-shot [`BasemindServer`] for the given repo `root` and `view`.
///
/// Mirrors the construction `cmd_serve` performs (config load, repo discover,
/// git cache open) but opens the store read-only and disables all background
/// facilities. `documents` flows the `#[command(flatten)]` document overrides
/// into the resolved config the same way `serve` does.
pub fn build_server(root: &Path, view: &str, documents: DocumentsCliOverrides) -> Result<BasemindServer> {
    build_server_for(root, view, documents, Intent::Read)
}

/// [`build_server`] with an explicit [`Intent`].
pub fn build_server_for(
    root: &Path,
    view: &str,
    documents: DocumentsCliOverrides,
    intent: Intent,
) -> Result<BasemindServer> {
    let daemon = super::rescan::daemon_is_up();
    let store = if daemon {
        Store::open_read_only_no_index(root, view).context("open store (blobs only; the daemon holds the index)")?
    } else if intent == Intent::Rescan {
        Store::open_with_holder(root, view, LockHolder::Rescan).map_err(|err| {
            if err.is_lock_contention() {
                CliExit::busy(crate::store::LOCK_CONTENTION_HELP)
            } else {
                anyhow::Error::new(err).context("open store (rescan)")
            }
        })?
    } else {
        Store::open_read_only(root, view).context("open store (read-only)")?
    };
    let basemind_dir = store.basemind_dir.clone();
    let cfg = Arc::new(load_config(root, documents)?);
    let repo = Repo::discover(root).ok().map(Arc::new);
    let git_cache = Arc::new(GitCache::open(&basemind_dir, CLI_GIT_CACHE_MEM, false).context("open git cache")?);
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if daemon {
        return Ok(BasemindServer::new_with_options(
            store,
            root.to_path_buf(),
            cfg,
            repo,
            git_cache,
            crate::mcp::ServerOptions {
                background: false,
                watch: false,
                read_only: true,
                daemon_writer: true,
                lazy_cache: true,
            },
        ));
    }
    Ok(BasemindServer::new_oneshot(
        store,
        root.to_path_buf(),
        cfg,
        repo,
        git_cache,
    ))
}

/// Load the resolved config, applying the document CLI override layer. Falls back
/// to defaults when no `basemind.toml` exists.
fn load_config(root: &Path, documents: DocumentsCliOverrides) -> Result<Config> {
    match config::load_with_overrides(root, None, Some(documents)) {
        Ok(loaded) => Ok(loaded.config),
        Err(config::ConfigError::NotFound(_)) => Ok(config::default_for_root(root)),
        Err(e) => Err(anyhow::anyhow!(e)),
    }
}
