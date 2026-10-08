//! Where a `references` / `callers` / `implementations` read runs.
//!
//! The three tools are range scans over fjall keyspaces (`calls_by_callee`, `calls_by_path`,
//! `implementations_by_trait`), and fjall's directory lock admits ONE process. So a session is in
//! one of four situations, and [`IndexRoute`] names which:
//!
//! * [`Local`](IndexRoute::Local) — this session holds the index (a writer, or `scan`/CLI).
//! * [`Host`](IndexRoute::Host) — a daemon-hosted connection; the daemon's own workspace pool
//!   holds the index, reached in-process.
//! * [`Daemon`](IndexRoute::Daemon) — a `daemon_writer` serve; the machine daemon holds the index,
//!   reached over its socket.
//! * [`InRam`](IndexRoute::InRam) — nothing holds an index we can reach (a second plain `serve`
//!   that lost the lock, or the daemon is down). The scan runs over a lazily built, byte-budgeted
//!   in-RAM projection of the blobs, which can be truncated and says so via the
//!   `projections_capped` notice.
//!
//! The first three answer from the same fjall index with the same scan code, so their results are
//! identical; only the last is lossy. A forward that fails (daemon restarting, socket error) falls
//! back to the projection rather than failing the tool.

use rmcp::ErrorData as McpError;

use super::MapCache;
use super::helpers_calls_scan::{CallRef, CallScanPage, calls_in_file_fjall, scan_calls_by_name, scan_calls_in_ram};
use super::helpers_impls::{ImplScanPage, scan_impls_in_ram, scan_impls_session};
use crate::index::IndexDb;
use crate::path::RelPath;

#[cfg(all(feature = "comms", any(unix, windows)))]
use {
    crate::comms::index_read_proto::{IndexReadQuery, IndexReadResult},
    std::path::PathBuf,
    std::sync::Arc,
};

/// How a session reaches the fjall index its reference reads need.
pub(super) enum IndexRoute {
    /// This session's own open index.
    Local(IndexDb),
    /// The daemon's workspace pool, in-process (daemon-hosted connection).
    #[cfg(all(feature = "comms", any(unix, windows)))]
    Host {
        host: Arc<dyn super::HostBackend>,
        root: PathBuf,
    },
    /// The machine daemon over its socket (`daemon_writer` serve).
    #[cfg(all(feature = "comms", any(unix, windows)))]
    Daemon {
        client: Arc<tokio::sync::Mutex<crate::comms::client::CommsClient>>,
        root: PathBuf,
    },
    /// No reachable index: scan the lazily built in-RAM projection.
    InRam,
}

impl IndexRoute {
    /// Decide the route for this session. Never fails: an unreachable daemon is the `InRam` route.
    pub(super) async fn resolve(state: &super::ServerState) -> Self {
        if let Some(idx) = state.shared.store.read().await.index_db.clone() {
            return Self::Local(idx);
        }
        #[cfg(all(feature = "comms", any(unix, windows)))]
        {
            if let Some(host) = &state.shared.host {
                return Self::Host {
                    host: Arc::clone(host),
                    root: state.shared.root.clone(),
                };
            }
            if state.shared.daemon_writer {
                match super::helpers_comms::resolve_comms_client(state, None).await {
                    Ok(client) => {
                        return Self::Daemon {
                            client,
                            root: state.shared.root.clone(),
                        };
                    }
                    Err(error) => {
                        tracing::warn!(%error, "daemon unreachable for a reference read; using the in-RAM projection");
                    }
                }
            }
        }
        Self::InRam
    }

    /// Run `query` on the daemon, or `None` when this route does not forward. `Some(Err)` is a
    /// transport or daemon-side failure the caller degrades past.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    async fn forward(&self, query: IndexReadQuery) -> Option<Result<IndexReadResult, String>> {
        match self {
            Self::Local(_) | Self::InRam => None,
            Self::Host { host, root } => {
                let host = Arc::clone(host);
                let root = root.clone();
                // ~keep The fjall scan is blocking, so it runs off the reactor.
                Some(
                    match tokio::task::spawn_blocking(move || host.host_index_read(&root, query)).await {
                        Ok(result) => result,
                        Err(join) => Err(format!("host index read panicked: {join}")),
                    },
                )
            }
            Self::Daemon { client, root } => Some(
                client
                    .lock()
                    .await
                    .index_read(root.clone(), query)
                    .await
                    .map_err(|error| error.to_string()),
            ),
        }
    }

    /// The `calls_by_callee` name scan behind `references` and `callers`.
    pub(super) async fn scan_calls(
        &self,
        cache: &MapCache,
        name: &str,
        limit: usize,
        cursor_after: Option<&[u8]>,
    ) -> Result<CallScanPage, McpError> {
        if let Self::Local(idx) = self {
            return scan_calls_by_name(idx, name, limit, cursor_after);
        }
        #[cfg(all(feature = "comms", any(unix, windows)))]
        match self
            .forward(IndexReadQuery::CallScan {
                name: name.to_owned(),
                limit: u32::try_from(limit).unwrap_or(u32::MAX),
                cursor: cursor_after.map(<[u8]>::to_vec),
            })
            .await
        {
            Some(Ok(IndexReadResult::CallScan(page))) => return Ok(super::index_read::call_page_from_wire(page)),
            Some(Ok(_)) => {
                tracing::warn!("daemon answered a call scan with the wrong reply; using the in-RAM projection")
            }
            Some(Err(error)) => tracing::warn!(%error, "forwarded call scan failed; using the in-RAM projection"),
            None => {}
        }
        Ok(blocking(|| {
            scan_calls_in_ram(cache.calls_projection(), name, limit, cursor_after)
        }))
    }

    /// Every call site of each of `paths`, in request order — the `callers` resolved refinement.
    pub(super) async fn calls_in_files(
        &self,
        cache: &MapCache,
        paths: &[RelPath],
    ) -> Result<Vec<Vec<CallRef>>, McpError> {
        if let Self::Local(idx) = self {
            return paths.iter().map(|path| calls_in_file_fjall(idx, path)).collect();
        }
        #[cfg(all(feature = "comms", any(unix, windows)))]
        match self
            .forward(IndexReadQuery::CallsInFiles { paths: paths.to_vec() })
            .await
        {
            Some(Ok(IndexReadResult::CallsInFiles(per_file))) if per_file.len() == paths.len() => {
                return Ok(per_file.into_iter().map(super::index_read::calls_from_wire).collect());
            }
            Some(Ok(_)) => tracing::warn!(
                "daemon answered a calls-in-files read with the wrong reply; using the in-RAM projection"
            ),
            Some(Err(error)) => tracing::warn!(%error, "forwarded calls-in-files failed; using the in-RAM projection"),
            None => {}
        }
        Ok(blocking(|| {
            let projection = cache.calls_projection();
            paths
                .iter()
                .map(|path| projection.calls_in_file(path).to_vec())
                .collect()
        }))
    }

    /// The `implementations_by_trait` scan behind `implementations`.
    pub(super) async fn scan_impls(
        &self,
        cache: &MapCache,
        trait_name: &str,
        language: Option<&str>,
        limit: usize,
        cursor_after: Option<&[u8]>,
    ) -> Result<ImplScanPage, McpError> {
        if let Self::Local(idx) = self {
            return scan_impls_session(idx, cache, trait_name, language, limit, cursor_after);
        }
        #[cfg(all(feature = "comms", any(unix, windows)))]
        match self
            .forward(IndexReadQuery::ImplScan {
                trait_name: trait_name.to_owned(),
                language: language.map(str::to_owned),
                limit: u32::try_from(limit).unwrap_or(u32::MAX),
                cursor: cursor_after.map(<[u8]>::to_vec),
            })
            .await
        {
            Some(Ok(IndexReadResult::ImplScan(page))) => return Ok(super::index_read::impl_page_from_wire(page)),
            Some(Ok(_)) => {
                tracing::warn!("daemon answered an impl scan with the wrong reply; using the in-RAM projection")
            }
            Some(Err(error)) => tracing::warn!(%error, "forwarded impl scan failed; using the in-RAM projection"),
            None => {}
        }
        Ok(blocking(|| {
            scan_impls_in_ram(
                cache.impls_projection(),
                cache,
                trait_name,
                language,
                limit,
                cursor_after,
            )
        }))
    }
}

/// Run work that may build the whole-corpus projection (rayon + blob IO) without stalling the
/// reactor. `block_in_place` needs the multi-thread runtime the CLI builds; on a `current_thread`
/// runtime (tests) it runs inline, which is safe because that runtime has no other task to starve.
fn blocking<R>(work: impl FnOnce() -> R) -> R {
    let multi_thread = tokio::runtime::Handle::try_current()
        .map(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    if multi_thread {
        tokio::task::block_in_place(work)
    } else {
        work()
    }
}
