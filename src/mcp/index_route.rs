//! Where a `references` / `callers` / `implementations` read runs.
//!
//! The three tools are range scans over fjall keyspaces (`calls_by_callee`, `calls_by_path`,
//! `implementations_by_trait`), and fjall's directory lock admits ONE process. So a session is in
//! one of three situations, and [`IndexRoute`] names which:
//!
//! * [`Local`](IndexRoute::Local) — this session holds the index (a writer, `scan`/CLI, or a
//!   daemon-hosted connection, whose store shares the pool writer's index handle).
//! * [`InRam`](IndexRoute::InRam) — nothing holds an index we can reach (a second plain `serve`
//!   that lost the lock, or the daemon is down). The scan runs over a lazily built, byte-budgeted
//!   in-RAM projection of the blobs, which can be truncated and says so via the
//!   `projections_capped` notice.
//!
//! Only the last is lossy.

use rmcp::ErrorData as McpError;

use super::MapCache;
use super::helpers_calls_scan::{CallRef, CallScanPage, calls_in_content_fjall, scan_calls_by_name, scan_calls_in_ram};
use super::helpers_impls::{ImplScanPage, scan_impls_in_ram, scan_impls_session};
use super::helpers_refs_match::Matching;
use crate::index::grep_bloom::GrepEntry;
use crate::index::keys::ContentKey;
use crate::index::view::{IndexView, StandaloneView};
use crate::path::RelPath;

/// How a session reaches the fjall index its reference reads need.
pub(super) enum IndexRoute {
    /// This session's own open index.
    Local(Box<dyn IndexView>),
    /// No reachable index: scan the lazily built in-RAM projection.
    InRam,
}

impl IndexRoute {
    /// Decide the route for this session. Never fails: an unreachable daemon is the `InRam` route.
    pub(super) async fn resolve(state: &super::ServerState) -> Self {
        if let Some(idx) = state.store_read().await.index_db.clone() {
            return Self::Local(Box::new(StandaloneView(idx)));
        }
        Self::InRam
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
