//! Forwarded-op handlers for the [`Broker`](super::daemon::Broker): the resolved-refs read plus the
//! CORE memory and PROPOSAL governance operations a `daemon_writer` serve ships to the daemon (the
//! machine's sole fjall writer). Split out of `daemon.rs` to keep it under the 1000-line
//! `rust-max-lines` cap; this is a second `impl Broker` block. The request dispatch stays in
//! `daemon.rs` and calls these methods on the same type.

use std::sync::Arc;

use super::daemon::Broker;
use super::protocol::CommsResponse;

impl Broker {
    /// Answer a forwarded precise resolved-reference read from the workspace's read-write fjall index
    /// (the daemon holds it as the sole writer, so the cross-file `refs_by_def` / `refs_by_path`
    /// edges a read-only serve cannot see are present here). The prefix scan is blocking, so it runs
    /// on a blocking thread. A pool/open error becomes a `CommsResponse::Error` (never a torn link).
    pub(super) async fn on_resolved_refs(
        &self,
        root: std::path::PathBuf,
        query: crate::comms::resolved_proto::ResolvedRefQuery,
    ) -> CommsResponse {
        self.mark_active().await;
        let pool = Arc::clone(&self.workspaces);
        match tokio::task::spawn_blocking(move || {
            pool.with_workspace(&root, |store| resolve_refs_against(store, &query))
        })
        .await
        {
            Ok(Ok(result)) => CommsResponse::ResolvedRefs(result),
            Ok(Err(error)) => CommsResponse::Error {
                code: "resolved_refs_failed".to_string(),
                message: error.to_string(),
            },
            Err(join) => CommsResponse::Error {
                code: "resolved_refs_panicked".to_string(),
                message: join.to_string(),
            },
        }
    }

    /// Answer a forwarded `references` / `callers` / `implementations` scan from the workspace's
    /// read-write fjall index. The scans are blocking range walks over whole keyspaces, so they run
    /// on a blocking thread. A pool/open error becomes a `CommsResponse::Error` (never a torn
    /// link), which the caller degrades past rather than mistaking for an empty result set.
    pub(super) async fn on_index_read(
        &self,
        root: std::path::PathBuf,
        query: crate::comms::index_read_proto::IndexReadQuery,
    ) -> CommsResponse {
        self.mark_active().await;
        let pool = Arc::clone(&self.workspaces);
        match tokio::task::spawn_blocking(move || index_read_via_pool(&pool, &root, &query)).await {
            Ok(Ok(result)) => CommsResponse::IndexRead(result),
            Ok(Err(message)) => CommsResponse::Error {
                code: "index_read_failed".to_string(),
                message,
            },
            Err(join) => CommsResponse::Error {
                code: "index_read_panicked".to_string(),
                message: join.to_string(),
            },
        }
    }

    /// Answer a forwarded code-search lane read from the workspace's read-write fjall index. Both
    /// lanes live only in fjall — BM25 postings and `symbols_by_name` — so a reader `serve` gets
    /// nothing from either; the daemon holds the sole writer handle and can. Ranking only: chunk
    /// bodies stay in content-addressed blobs the caller reads itself. The scans are blocking, so
    /// they run on a blocking thread. A pool/open error becomes a `CommsResponse::Error` (never a
    /// torn link), which the caller surfaces rather than mistaking for an empty result set.
    /// The request variant exists whenever `comms` does, so the wire shape does not vary by feature
    /// — but the lanes themselves are `code-search` code. A daemon built without it answers with an
    /// explicit error rather than empty lanes, so the caller reports "cannot search" instead of
    /// "found nothing".
    #[cfg(not(feature = "code-search"))]
    pub(super) async fn on_code_search_lanes(
        &self,
        _root: std::path::PathBuf,
        _query: crate::comms::code_search_proto::CodeSearchLaneQuery,
    ) -> CommsResponse {
        CommsResponse::Error {
            code: "code_search_unavailable".to_string(),
            message: "this daemon was built without the `code-search` feature, so it holds no BM25 \
                      index and cannot answer the keyword or exact lane"
                .to_string(),
        }
    }

    #[cfg(feature = "code-search")]
    pub(super) async fn on_code_search_lanes(
        &self,
        root: std::path::PathBuf,
        query: crate::comms::code_search_proto::CodeSearchLaneQuery,
    ) -> CommsResponse {
        self.mark_active().await;
        let pool = Arc::clone(&self.workspaces);
        match tokio::task::spawn_blocking(move || {
            pool.with_workspace(&root, |store| code_search_lanes_against(store, &query))
        })
        .await
        {
            Ok(Ok(Ok(result))) => CommsResponse::CodeSearchLanes(result),
            Ok(Ok(Err(unavailable))) => CommsResponse::Error {
                code: "code_search_index_unavailable".to_string(),
                message: unavailable,
            },
            Ok(Err(error)) => CommsResponse::Error {
                code: "code_search_lanes_failed".to_string(),
                message: error.to_string(),
            },
            Err(join) => CommsResponse::Error {
                code: "code_search_lanes_panicked".to_string(),
                message: join.to_string(),
            },
        }
    }

    /// Run a forwarded CORE memory operation against the workspace's read-write index. The daemon is
    /// the sole fjall writer, and the pool's per-workspace store lock serializes same-workspace ops,
    /// making the forwarded `memory_put` read-modify-write atomic (no per-key lock needed here). The
    /// fjall work is blocking, so it runs on a blocking thread. Any error becomes a
    /// `CommsResponse::Error` (never a torn link).
    #[cfg(feature = "memory")]
    pub(super) async fn on_memory(
        &self,
        root: std::path::PathBuf,
        scope: String,
        op: super::memory_proto::MemoryOp,
    ) -> CommsResponse {
        self.mark_active().await;
        let pool = Arc::clone(&self.workspaces);
        let outcome = tokio::task::spawn_blocking(move || {
            pool.with_workspace_mut(&root, |store| {
                let idx = store
                    .index_db
                    .as_ref()
                    .ok_or(crate::mcp::memory_ops::MemoryOpError::IndexUnavailable)?;
                crate::mcp::memory_ops::run_memory_op(idx, &scope, &op)
            })
        })
        .await;
        match outcome {
            Ok(Ok(Ok(outcome))) => CommsResponse::Memory(outcome),
            Ok(Ok(Err(error))) => CommsResponse::Error {
                code: "memory_op_failed".to_string(),
                message: error.to_string(),
            },
            Ok(Err(error)) => CommsResponse::Error {
                code: "memory_workspace_failed".to_string(),
                message: error.to_string(),
            },
            Err(join) => CommsResponse::Error {
                code: "memory_panicked".to_string(),
                message: join.to_string(),
            },
        }
    }

    /// Run a forwarded PROPOSAL governance operation against the workspace's read-write index. Same
    /// contract as [`on_memory`](Self::on_memory): the daemon is the sole fjall writer, the pool's
    /// per-workspace store lock serializes same-workspace ops (so the mine-apply tombstone-check +
    /// insert see one consistent view), the fjall work runs on a blocking thread, and any error
    /// becomes a `CommsResponse::Error` (never a torn link).
    #[cfg(feature = "memory")]
    pub(super) async fn on_governance(
        &self,
        root: std::path::PathBuf,
        scope: String,
        op: super::proposals_proto::GovernanceOp,
    ) -> CommsResponse {
        self.mark_active().await;
        let pool = Arc::clone(&self.workspaces);
        let outcome = tokio::task::spawn_blocking(move || {
            pool.with_workspace_mut(&root, |store| {
                let idx = store
                    .index_db
                    .as_ref()
                    .ok_or(crate::mcp::memory_ops::MemoryOpError::IndexUnavailable)?;
                crate::mcp::proposals_ops::run_governance_op(idx, &scope, &op)
            })
        })
        .await;
        match outcome {
            Ok(Ok(Ok(outcome))) => CommsResponse::Governance(outcome),
            Ok(Ok(Err(error))) => CommsResponse::Error {
                code: "governance_op_failed".to_string(),
                message: error.to_string(),
            },
            Ok(Err(error)) => CommsResponse::Error {
                code: "governance_workspace_failed".to_string(),
                message: error.to_string(),
            },
            Err(join) => CommsResponse::Error {
                code: "governance_panicked".to_string(),
                message: join.to_string(),
            },
        }
    }
}

/// Run a forwarded [`IndexReadQuery`](crate::comms::index_read_proto::IndexReadQuery) against the
/// pool's workspace.
///
/// The name scan walks the whole `calls_by_callee` partition, so it takes only a cloned
/// [`IndexDb`](crate::index::IndexDb) handle out of the workspace lock and scans outside it: holding
/// the per-workspace store lock for that long would stall a rescan behind a read. The other two
/// reads are bounded (a capped file batch, a small keyspace) and run under the lock because they
/// also need the store's file entries and blobs.
pub(crate) fn index_read_via_pool(
    pool: &super::workspace_pool::WorkspacePool,
    root: &std::path::Path,
    query: &crate::comms::index_read_proto::IndexReadQuery,
) -> Result<crate::comms::index_read_proto::IndexReadResult, String> {
    use crate::comms::index_read_proto::IndexReadQuery;
    if let IndexReadQuery::CallScan { name, limit, cursor } = query {
        let idx = pool
            .with_workspace(root, |store| store.index_db.clone())
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "the daemon holds this workspace without a fjall index".to_string())?;
        return crate::mcp::index_read::call_scan(&idx, name, *limit, cursor.as_deref());
    }
    pool.with_workspace(root, |store| crate::mcp::index_read::index_read_against(store, query))
        .map_err(|error| error.to_string())?
}

/// Answer a [`ResolvedRefQuery`] against an open workspace store. Delegates to the shared
/// `crate::query` resolvers, which read the fjall `refs_by_def` / `refs_by_path` partitions when the
/// index is open — as it always is on the daemon (sole writer, opens read-write) — so the reply
/// carries the full cross-file edge set. A degraded index (no fjall) falls back to intra-file blobs.
pub(crate) fn resolve_refs_against(
    store: &crate::store::Store,
    query: &crate::comms::resolved_proto::ResolvedRefQuery,
) -> crate::comms::resolved_proto::ResolvedRefResult {
    use crate::comms::resolved_proto::{ResolvedRefQuery, ResolvedRefResult};
    match query {
        ResolvedRefQuery::ReferencesTo { def_path, def_start } => {
            ResolvedRefResult::References(crate::query::resolved_references(store, def_path, *def_start))
        }
        ResolvedRefQuery::DefinitionOf { use_path, use_start } => {
            ResolvedRefResult::Definition(crate::query::definition_of(store, use_path, *use_start))
        }
    }
}

/// Answer a [`CodeSearchLaneQuery`](crate::comms::code_search_proto::CodeSearchLaneQuery) against an
/// open workspace store.
///
/// Errors — rather than returning empty lanes — when the store has no `IndexDb`. On the daemon that
/// should be unreachable, since it opens every workspace read-write as the sole writer; but if it
/// ever happens, empty-and-successful would recreate on this side the exact silent failure the
/// forward exists to remove, on both the socket and `HostBackend` paths. The caller can only tell
/// "searched, found nothing" from "could not search" if the second one is an error.
///
/// `limit` is clamped to [`MAX_FORWARDED_LANE_LIMIT`]: it arrives from the wire, and both scans run
/// while the pool's per-workspace store lock is held, so an unbounded value would let one client
/// pin that lock and drag back a corpus-sized `exact` list.
#[cfg(feature = "code-search")]
pub(crate) fn code_search_lanes_against(
    store: &crate::store::Store,
    query: &crate::comms::code_search_proto::CodeSearchLaneQuery,
) -> Result<crate::comms::code_search_proto::CodeSearchLaneResult, String> {
    use crate::comms::code_search_proto::CodeSearchLaneResult;

    let limit = (query.limit as usize).min(MAX_FORWARDED_LANE_LIMIT);
    let Some(db) = store.index_db.as_ref() else {
        return Err(
            "the daemon holds this workspace without a fjall index, so neither the keyword nor the \
             exact lane can be read"
                .to_string(),
        );
    };
    let keyword = crate::search::bm25::bm25_search(db, &query.query, limit)
        .into_iter()
        .map(|hit| (hit.chunk_id, hit.score))
        .collect();
    let exact = if query.want_exact {
        crate::search::exact::exact_lane_chunk_ids(store, db, &query.query, limit)
    } else {
        Vec::new()
    };
    Ok(CodeSearchLaneResult { keyword, exact })
}

/// Ceiling on a forwarded lane's per-lane result count, matching the caller's own RRF fusion cap.
/// Enforced daemon-side because the value is attacker-controllable over the socket.
#[cfg(feature = "code-search")]
pub(crate) const MAX_FORWARDED_LANE_LIMIT: usize = 200;
