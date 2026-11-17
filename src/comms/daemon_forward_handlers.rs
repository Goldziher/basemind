//! The daemon pool's answers to the reads a daemon-hosted session cannot make on its own store,
//! which holds no index: reference scans, resolved references and the code-search lanes. The
//! hosted session reaches them in process through the pool ([`crate::mcp::HostBackend`]).

/// Run a forwarded [`IndexReadQuery`](crate::comms::index_read_proto::IndexReadQuery) against the
/// pool's workspace.
///
/// The name scan walks the whole `calls_by_callee` partition and the grep bloom test touches a row and
/// a file per path, so both take only a cloned
/// [`IndexDb`](crate::index::IndexDb) handle out of the workspace lock and scan outside it: holding
/// the per-workspace store lock for that long would stall a rescan behind a read. The other two
/// reads are bounded (a capped file batch, a small keyspace) and run under the lock because they
/// also need the store's file entries and blobs.
pub(crate) fn index_read_via_pool(
    pool: &super::workspace_pool::WorkspacePool,
    root: &std::path::Path,
    query: &crate::comms::index_read_proto::IndexReadQuery,
) -> Result<crate::comms::index_read_proto::IndexReadResult, String> {
    use crate::comms::index_read_proto::IndexReadQuery;
    if let IndexReadQuery::CallScan {
        name,
        exact,
        limit,
        cursor,
    } = query
    {
        let idx = pool
            .with_workspace(root, |store| store.read_index_view())
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "the daemon holds this workspace without a fjall index".to_string())?;
        let matching = crate::mcp::index_read::Matching::from_substring(!exact);
        return crate::mcp::index_read::call_scan(&*idx, name, matching, *limit, cursor.as_deref());
    }
    if let IndexReadQuery::GrepBloom { pattern, entries } = query {
        let (idx, store_root) = pool
            .with_workspace(root, |store| (store.read_index_view(), store.root.clone()))
            .map_err(|error| error.to_string())?;
        let idx = idx.ok_or_else(|| "the daemon holds this workspace without a fjall index".to_string())?;
        return crate::mcp::index_read::grep_skip(&*idx, &store_root, pattern, entries);
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
            // ~keep Read before the edge: a rescan that lands between this and the edge read then
            // ~keep pairs a new edge with the old use hash, which the client refuses, never an old
            // ~keep edge with the new hash.
            let use_hash = store.lookup(use_path).map(|entry| entry.hash_hex.clone());
            let settled =
                crate::store::read_status_sidecar(&store.basemind_dir).is_none_or(|status| !status.is_degraded());
            ResolvedRefResult::Definition(crate::query::definition_of(store, use_path, *use_start).map(
                |(path, start)| crate::comms::resolved_proto::ResolvedDefinition {
                    landing_hash: store.lookup(&path).map(|entry| entry.hash_hex.clone()),
                    use_hash,
                    settled,
                    path,
                    start,
                },
            ))
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
    let Some(db) = store.read_index_view() else {
        return Err(
            "the daemon holds this workspace without a fjall index, so neither the keyword nor the \
             exact lane can be read"
                .to_string(),
        );
    };
    let keyword = crate::search::bm25::bm25_search(&*db, &query.query, limit)
        .into_iter()
        .map(|hit| (hit.chunk_id, hit.score))
        .collect();
    let exact = if query.want_exact {
        crate::search::exact::exact_lane_chunk_ids(store, &*db, &query.query, limit)
    } else {
        Vec::new()
    };
    Ok(CodeSearchLaneResult { keyword, exact })
}

/// Ceiling on a forwarded lane's per-lane result count, matching the caller's own RRF fusion cap.
/// Enforced daemon-side because the value is attacker-controllable over the socket.
#[cfg(feature = "code-search")]
pub(crate) const MAX_FORWARDED_LANE_LIMIT: usize = 200;
