//! Where a `references` / `callers` / `implementations` read runs.
//!
//! The three tools are range scans over fjall keyspaces (`calls_by_callee`, `calls_by_path`,
//! `implementations_by_trait`), and fjall's directory lock admits ONE process. So a session is in
//! one of three situations, and [`IndexRoute`] names which:
//!
//! * [`Local`](IndexRoute::Local) — this session holds the index (a writer, or `scan`/CLI).
//! * [`Host`](IndexRoute::Host) — a daemon-hosted connection; the daemon's own workspace pool
//!   holds the index, reached in-process.
//! * [`InRam`](IndexRoute::InRam) — nothing holds an index we can reach (a second plain `serve`
//!   that lost the lock, or the daemon is down). The scan runs over a lazily built, byte-budgeted
//!   in-RAM projection of the blobs, which can be truncated and says so via the
//!   `projections_capped` notice.
//!
//! The first two answer from the same fjall index with the same scan code, so their results are
//! identical; only the last is lossy. A forward that fails (daemon restarting, socket error) falls
//! back to the projection rather than failing the tool.

use rmcp::ErrorData as McpError;

use super::MapCache;
use super::helpers_calls_scan::{CallRef, CallScanPage, calls_in_content_fjall, scan_calls_by_name, scan_calls_in_ram};
use super::helpers_impls::{ImplScanPage, scan_impls_in_ram, scan_impls_session};
use super::helpers_refs_match::Matching;
use crate::index::grep_bloom::GrepEntry;
use crate::index::keys::ContentKey;
use crate::index::view::{IndexView, StandaloneView};
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
    Local(Box<dyn IndexView>),
    /// The daemon's workspace pool, in-process (daemon-hosted connection).
    #[cfg(all(feature = "comms", any(unix, windows)))]
    Host {
        host: Arc<dyn super::HostBackend>,
        root: PathBuf,
    },
    /// No reachable index: scan the lazily built in-RAM projection.
    InRam,
}

impl IndexRoute {
    /// Decide the route for this session. Never fails: an unreachable daemon is the `InRam` route.
    pub(super) async fn resolve(state: &super::ServerState) -> Self {
        if let Some(idx) = state.store_read().await.index_db.clone() {
            return Self::Local(Box::new(StandaloneView(idx)));
        }
        #[cfg(all(feature = "comms", any(unix, windows)))]
        {
            if let Some(host) = &state.host {
                return Self::Host {
                    host: Arc::clone(host),
                    root: state.shared.root.clone(),
                };
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
        }
    }

    /// Which of `entries` provably cannot match the grep `pattern` (`true` = skip), per the per-file
    /// trigram blooms. `None` means "no prefilter available" (no reachable index, a forward that
    /// failed, an unusable reply): the caller then sweeps every file, which is always correct.
    pub(super) async fn grep_skip(
        &self,
        root: &std::path::Path,
        pattern: &str,
        entries: &[GrepEntry],
    ) -> Option<Vec<bool>> {
        if let Self::Local(idx) = self {
            let needle = crate::index::grep_bloom::Needle::compile(pattern)?;
            return Some(blocking(|| idx.grep_skip_verdicts(root, entries, &needle)));
        }
        #[cfg(all(feature = "comms", any(unix, windows)))]
        {
            // Compiled here too so a pattern with no usable literal costs no round trip.
            crate::index::grep_bloom::Needle::compile(pattern)?;
            let mut skip = Vec::with_capacity(entries.len());
            for chunk in entries.chunks(crate::comms::index_read_proto::MAX_GREP_BLOOM_PATHS) {
                let query = IndexReadQuery::GrepBloom {
                    pattern: pattern.to_owned(),
                    entries: chunk.to_vec(),
                };
                match self.forward(query).await {
                    Some(Ok(IndexReadResult::GrepSkip(part))) if part.len() == chunk.len() => skip.extend(part),
                    Some(Ok(_)) => {
                        tracing::warn!("daemon answered a grep bloom test with the wrong reply; sweeping every file");
                        return None;
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%error, "forwarded grep bloom test failed; sweeping every file");
                        return None;
                    }
                    None => return None,
                }
            }
            Some(skip)
        }
        #[cfg(not(all(feature = "comms", any(unix, windows))))]
        None
    }

    /// The `calls_by_callee` name scan behind `references` and `callers`, with the generation of the
    /// key order it was cut in: of the live keys of the index that ran it (this session's, or the
    /// daemon's), of the in-RAM map's order keys when the projection ran it. A `references` /
    /// `callers` cursor carries it, and is refused as stale once it changes, since an edit moves the
    /// edited file's hits in that order.
    pub(super) async fn scan_calls(
        &self,
        cache: &MapCache,
        name: &str,
        matching: Matching,
        limit: usize,
        cursor_after: Option<&[u8]>,
    ) -> Result<(CallScanPage, u64), McpError> {
        if let Self::Local(idx) = self {
            let generation = idx.live_keys().fingerprint();
            let page = blocking(|| scan_calls_by_name(&**idx, name, matching, limit, cursor_after))?;
            return Ok((page, generation));
        }
        #[cfg(all(feature = "comms", any(unix, windows)))]
        match self
            .forward(IndexReadQuery::CallScan {
                name: name.to_owned(),
                exact: matching == Matching::Exact,
                limit: u32::try_from(limit).unwrap_or(u32::MAX),
                cursor: cursor_after.map(<[u8]>::to_vec),
            })
            .await
        {
            Some(Ok(IndexReadResult::CallScan(page))) => {
                let generation = page.generation;
                let mut page = super::index_read::call_page_from_wire(page);
                // A daemon that predates `exact` ignores it and answers with substring hits. Only
                // this page's share of them can be dropped here, so `retain` marks the total partial.
                page.retain(|hit| matching.accepts_callee(&hit.callee, name));
                return Ok((page, generation));
            }
            Some(Ok(_)) => {
                tracing::warn!("daemon answered a call scan with the wrong reply; using the in-RAM projection")
            }
            Some(Err(error)) => tracing::warn!(%error, "forwarded call scan failed; using the in-RAM projection"),
            None => {}
        }
        let page = blocking(|| scan_calls_in_ram(cache.calls_projection(), name, matching, limit, cursor_after));
        Ok((page, cache.content_fingerprint()))
    }

    /// The content key each of `paths` is read under, in request order: from one snapshot of the
    /// view's live keys on a local index, else the in-RAM map's order key
    /// ([`FileMeta::order_key`](crate::engine::l1_cache::FileMeta::order_key)), `None` for a file the
    /// map lacks. A forwarded read replaces them with the daemon's ([`Self::calls_in_files`]).
    pub(super) fn content_keys(&self, cache: &MapCache, paths: &[RelPath]) -> Vec<Option<ContentKey>> {
        if let Self::Local(idx) = self {
            let live = idx.live_keys();
            return paths.iter().map(|path| live.key_of(path).copied()).collect();
        }
        paths
            .iter()
            .map(|path| cache.meta(path).map(crate::engine::l1_cache::FileMeta::order_key))
            .collect()
    }

    /// Every call site of each of `files`, in request order, with the content key it was read under:
    /// the order key of the file's calls, which places a resolved caller among the scan hits. `None`
    /// for a file read under no key, which has no calls. A local index and the in-RAM projection
    /// read each file under the key given with it (a hit key's, or [`Self::content_keys`]'); a
    /// forwarded read reads by path under the daemon's live keys and returns the daemon's key.
    pub(super) async fn calls_in_files(
        &self,
        cache: &MapCache,
        files: &[(RelPath, Option<ContentKey>)],
    ) -> Result<Vec<Option<(ContentKey, Vec<CallRef>)>>, McpError> {
        if let Self::Local(idx) = self {
            return files
                .iter()
                .map(|(_, content)| {
                    let Some(content) = content else {
                        return Ok(None);
                    };
                    calls_in_content_fjall(&**idx, Some(content)).map(|calls| Some((*content, calls)))
                })
                .collect();
        }
        #[cfg(all(feature = "comms", any(unix, windows)))]
        match self
            .forward(IndexReadQuery::CallsInFiles {
                paths: files.iter().map(|(path, _)| path.clone()).collect(),
            })
            .await
        {
            Some(Ok(IndexReadResult::CallsInFiles(per_file))) if per_file.len() == files.len() => {
                return Ok(per_file
                    .into_iter()
                    .map(super::index_read::file_calls_from_wire)
                    .collect());
            }
            Some(Ok(_)) => tracing::warn!(
                "daemon answered a calls-in-files read with the wrong reply; using the in-RAM projection"
            ),
            Some(Err(error)) => tracing::warn!(%error, "forwarded calls-in-files failed; using the in-RAM projection"),
            None => {}
        }
        Ok(blocking(|| {
            let projection = cache.calls_projection();
            files
                .iter()
                .map(|(path, content)| content.map(|content| (content, projection.calls_in_file(path).to_vec())))
                .collect()
        }))
    }

    /// The `implementations_by_trait` scan behind `implementations`.
    pub(super) async fn scan_impls(
        &self,
        cache: &MapCache,
        trait_name: &str,
        matching: Matching,
        language: Option<&str>,
        limit: usize,
        cursor_after: Option<&[u8]>,
    ) -> Result<ImplScanPage, McpError> {
        if let Self::Local(idx) = self {
            return scan_impls_session(&**idx, cache, trait_name, matching, language, limit, cursor_after);
        }
        #[cfg(all(feature = "comms", any(unix, windows)))]
        match self
            .forward(IndexReadQuery::ImplScan {
                trait_name: trait_name.to_owned(),
                exact: matching == Matching::Exact,
                language: language.map(str::to_owned),
                limit: u32::try_from(limit).unwrap_or(u32::MAX),
                cursor: cursor_after.map(<[u8]>::to_vec),
            })
            .await
        {
            Some(Ok(IndexReadResult::ImplScan(page))) => {
                let mut page = super::index_read::impl_page_from_wire(page);
                page.retain(|hit| matching.accepts_trait(&hit.trait_name, trait_name));
                return Ok(page);
            }
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
                matching,
                language,
                limit,
                cursor_after,
            )
        }))
    }
}

/// Run blocking work (a corpus sweep, blob or file IO, a git walk) without stalling the
/// reactor. `block_in_place` needs the multi-thread runtime the CLI builds; on a `current_thread`
/// runtime (tests) it runs inline, which is safe because that runtime has no other task to starve.
pub(super) fn blocking<R>(work: impl FnOnce() -> R) -> R {
    #[cfg(test)]
    BLOCKING_CALLS.with(|calls| calls.set(calls.get() + 1));
    let multi_thread = tokio::runtime::Handle::try_current()
        .map(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    if multi_thread {
        tokio::task::block_in_place(work)
    } else {
        work()
    }
}

#[cfg(test)]
thread_local! {
    static BLOCKING_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many times [`blocking`] was entered on this thread: what a test reads to see that a call
/// site still goes through it.
#[cfg(all(test, feature = "memory"))]
pub(super) fn blocking_calls() -> usize {
    BLOCKING_CALLS.with(std::cell::Cell::get)
}
