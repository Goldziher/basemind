//! The engine a CLI command runs on.
//!
//! [`CliContext`] opens a one-shot `LocalEngine` with every background facility disabled, so a
//! single command runs the engine operation an MCP client would reach, then the process exits. The
//! store is opened read-only so the CLI never contends for the `.basemind/.lock` flock a running
//! `basemind serve` may already hold.
//!
//! When the comms daemon is up it is the sole index writer and holds the fjall lock, so a command on
//! the working view opens no store at all: [`CliContext`] forwards the whole call to the daemon over one
//! relay session (`super::forward`) and renders what the daemon's tool body answered.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config::overrides::DocumentsCliOverrides;
use crate::config::{self, Config};
use crate::dispatch::LocalEngine;
use crate::git::Repo;
use crate::git_cache::GitCache;
use crate::store::Store;

/// Where a CLI command's call runs: in this process, or in the daemon that holds the index.
pub struct CliContext {
    root: std::path::PathBuf,
    backend: Backend,
}

enum Backend {
    Local(Box<LocalEngine>),
    #[cfg(all(feature = "comms", any(unix, windows)))]
    Daemon(super::forward::DaemonSession),
}

impl CliContext {
    /// The repository root the command runs against.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The in-process engine, for a command that is not a tool call (`admin telemetry`, `eval`).
    pub(crate) fn local(&self) -> Option<&LocalEngine> {
        match &self.backend {
            Backend::Local(server) => Some(server),
            #[cfg(all(feature = "comms", any(unix, windows)))]
            Backend::Daemon(_) => None,
        }
    }

    /// Run the tool `name` with `params`: through `local` in this process, or as `tools/call {name,
    /// params}` in the daemon. Both answer from the same tool body.
    pub(crate) async fn call<P: serde::Serialize>(
        &self,
        name: &str,
        params: P,
        local: impl AsyncFnOnce(&LocalEngine, P) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        #[cfg(not(all(feature = "comms", any(unix, windows))))]
        let _ = name;
        match &self.backend {
            Backend::Local(server) => local(server, params).await,
            #[cfg(all(feature = "comms", any(unix, windows)))]
            Backend::Daemon(session) => {
                let arguments = serde_json::to_value(&params).map_err(|error| {
                    rmcp::ErrorData::internal_error(format!("encode `{name}` arguments: {error}"), None)
                })?;
                session.call(name, arguments).await
            }
        }
    }
}

/// Which calls of a CLI command may go to the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Forward {
    /// None: the command is not a tool call (`admin telemetry`, `admin eval`).
    Never,
    /// A call on the working view, the only view the daemon hosts.
    WorkingView,
    /// A call that reads only the repository, never a view's index, so its answer is the same on
    /// every view: the daemon's working-view session answers it whatever `--view` names.
    AnyView,
}

/// The backend for one CLI command: the daemon when it is up and `forward` allows it for `view`,
/// else an in-process server.
pub async fn build_tools(
    root: &Path,
    view: &str,
    documents: DocumentsCliOverrides,
    forward: Forward,
) -> Result<CliContext> {
    let forwarded = match forward {
        Forward::Never => false,
        Forward::WorkingView => view == crate::store::VIEW_WORKING,
        Forward::AnyView => true,
    };
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if forwarded && super::rescan::daemon_is_up() {
        return Ok(CliContext {
            root: root.to_path_buf(),
            backend: Backend::Daemon(super::forward::DaemonSession::open(root).await?),
        });
    }
    let _ = forwarded;
    Ok(CliContext {
        root: root.to_path_buf(),
        backend: Backend::Local(Box::new(build_engine(root, view, documents)?)),
    })
}

/// LRU capacity per git-cache category for one-shot CLI invocations. Small —
/// a CLI process issues a single query and exits, so a big LRU never pays off.
const CLI_GIT_CACHE_MEM: usize = 256;

/// Construct a one-shot `LocalEngine` for the given repo `root` and `view`.
///
/// Mirrors the construction `cmd_serve` performs (config load, repo discover,
/// git cache open) but opens the store read-only and disables all background
/// facilities. With the daemon up the daemon holds the index, so the store is blobs only and the
/// engine writes nothing. `documents` flows the `#[command(flatten)]` document overrides
/// into the resolved config the same way `serve` does.
pub(crate) fn build_engine(root: &Path, view: &str, documents: DocumentsCliOverrides) -> Result<LocalEngine> {
    let daemon = super::rescan::daemon_is_up();
    let store = if daemon {
        Store::open_read_only_no_index(root, view).context("open store (blobs only; the daemon holds the index)")?
    } else {
        Store::open_for_query(root, view).context("open store (read-only)")?
    };
    // A query or a hook would otherwise answer empty with nothing to say why.
    if store.rescan_owed() {
        eprintln!("basemind: {}", crate::store::RESCAN_OWED);
    }
    let basemind_dir = store.basemind_dir.clone();
    let cfg = Arc::new(load_config(root, documents)?);
    let repo = Repo::discover(root).ok().map(Arc::new);
    let git_cache = Arc::new(GitCache::open(&basemind_dir, CLI_GIT_CACHE_MEM, false).context("open git cache")?);
    // ~keep Next to the daemon only a command that is not forwarded gets here: a view the daemon does
    // ~keep not host, or `admin telemetry` / `admin eval`. It reads the blobs and writes nothing.
    Ok(LocalEngine::open(
        store,
        root.to_path_buf(),
        cfg,
        repo,
        git_cache,
        daemon,
    ))
}

/// Load the resolved config, applying the document CLI override layer. Falls back
/// to defaults when no `basemind.toml` exists.
fn load_config(root: &Path, documents: DocumentsCliOverrides) -> Result<Config> {
    match config::overrides::load_with_overrides(root, None, Some(documents)) {
        Ok(loaded) => Ok(loaded.config),
        Err(config::ConfigError::NotFound(_)) => Ok(config::default_for_root(root)),
        Err(e) => Err(anyhow::anyhow!(e)),
    }
}
