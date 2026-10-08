//! Daemon-side executor for the forwarded reference reads, plus the page <-> wire conversions both
//! ends share.
//!
//! A `daemon_writer` serve cannot open the fjall index, so its `references` / `callers` /
//! `implementations` scans are shipped to the daemon as an
//! [`IndexReadQuery`](crate::comms::index_read_proto::IndexReadQuery). [`index_read_against`] runs
//! them against the daemon's read-write store by calling the very same scan functions a writer
//! session calls (`scan_calls_by_name`, `calls_in_file_fjall`, `scan_impls_fjall`) — which is what
//! makes the two paths equal by construction rather than by a test that has to keep up.

#![cfg(all(feature = "comms", any(unix, windows)))]

use super::cursor::Cursor;
use super::helpers::SEARCH_LIMIT_MAX;
use super::helpers_calls_scan::{CallRef, CallScanPage, calls_in_file_fjall, scan_calls_by_name};
use super::helpers_impls::{ImplScanPage, scan_impls_fjall};
use super::types::ReferenceHit;
use super::types_impls::ImplementationHit;
use crate::comms::index_read_proto::{
    IndexReadQuery, IndexReadResult, MAX_CALLS_IN_FILES, WireCall, WireCallHit, WireCallScan, WireImplHit, WireImplScan,
};
use crate::index::IndexDb;
use crate::store::Store;

/// Why a forwarded scan could not run: the workspace the daemon holds has no fjall index, so the
/// answer would be silently empty. Reported as an error so the caller can tell "searched, found
/// nothing" from "could not search".
const NO_INDEX: &str = "the daemon holds this workspace without a fjall index, so the reference scan cannot run";

/// Run a forwarded scan against an open workspace store.
pub(crate) fn index_read_against(store: &Store, query: &IndexReadQuery) -> Result<IndexReadResult, String> {
    let idx = store.index_db.as_ref().ok_or_else(|| NO_INDEX.to_string())?;
    match query {
        IndexReadQuery::CallScan { name, limit, cursor } => call_scan(idx, name, *limit, cursor.as_deref()),
        IndexReadQuery::CallsInFiles { paths } => calls_in_files(idx, paths),
        IndexReadQuery::ImplScan {
            trait_name,
            language,
            limit,
            cursor,
        } => {
            let page = scan_impls_fjall(
                idx,
                trait_name,
                language.as_deref(),
                clamp_limit(*limit),
                cursor.as_deref(),
                |rel, lang| store.lookup(rel).is_some_and(|entry| entry.language == lang),
                |rel, start_byte| {
                    store
                        .lookup(rel)
                        .and_then(|entry| store.read_l1_by_hex(&entry.hash_hex).ok().flatten())
                        .map_or((0, 0), |l1| super::helpers_impls::impl_row_col(&l1, start_byte))
                },
            )
            .map_err(|error| error.message.to_string())?;
            Ok(IndexReadResult::ImplScan(impl_page_to_wire(page)))
        }
    }
}

/// The `CallScan` arm on its own, so the daemon can run it holding only a cloned [`IndexDb`]
/// handle instead of the workspace store lock: this scan walks the whole `calls_by_callee`
/// partition and would otherwise block a rescan for its duration.
pub(crate) fn call_scan(
    idx: &IndexDb,
    name: &str,
    limit: u32,
    cursor: Option<&[u8]>,
) -> Result<IndexReadResult, String> {
    let page = scan_calls_by_name(idx, name, clamp_limit(limit), cursor).map_err(|error| error.message.to_string())?;
    Ok(IndexReadResult::CallScan(call_page_to_wire(page)))
}

fn calls_in_files(idx: &IndexDb, paths: &[crate::path::RelPath]) -> Result<IndexReadResult, String> {
    if paths.len() > MAX_CALLS_IN_FILES {
        return Err(format!(
            "calls_in_files names {} files; at most {MAX_CALLS_IN_FILES} per request",
            paths.len()
        ));
    }
    let per_file = paths
        .iter()
        .map(|path| {
            calls_in_file_fjall(idx, path).map(|calls| {
                calls
                    .into_iter()
                    .map(|c| WireCall {
                        callee: c.callee,
                        start_byte: c.start_byte,
                        line: c.line,
                        column: c.column,
                    })
                    .collect()
            })
        })
        .collect::<Result<Vec<Vec<WireCall>>, _>>()
        .map_err(|error| error.message.to_string())?;
    Ok(IndexReadResult::CallsInFiles(per_file))
}

/// The limit arrives off the wire, so it is clamped to the tool's own ceiling.
fn clamp_limit(limit: u32) -> usize {
    (limit as usize).min(SEARCH_LIMIT_MAX as usize)
}

fn call_page_to_wire(page: CallScanPage) -> WireCallScan {
    let has_more = page.next_cursor.is_some();
    let hits = page
        .hits
        .into_iter()
        .zip(page.hit_keys)
        .zip(page.hit_starts)
        .map(|((hit, key), start_byte)| WireCallHit {
            key,
            path: hit.path,
            line: hit.line,
            column: hit.column,
            callee: hit.callee,
            start_byte,
        })
        .collect();
    WireCallScan {
        total: page.total,
        total_is_partial: page.total_is_partial,
        has_more,
        hits,
    }
}

/// Rebuild the scan page a writer session would have produced locally. The cursor is derived the
/// same way the scan derives it: the last hit's key, present only when more matches remain.
pub(super) fn call_page_from_wire(wire: WireCallScan) -> CallScanPage {
    let mut hits = Vec::with_capacity(wire.hits.len());
    let mut hit_keys = Vec::with_capacity(wire.hits.len());
    let mut hit_starts = Vec::with_capacity(wire.hits.len());
    for hit in wire.hits {
        hits.push(ReferenceHit {
            path: hit.path,
            line: hit.line,
            column: hit.column,
            callee: hit.callee,
            resolved: None,
        });
        hit_keys.push(hit.key);
        hit_starts.push(hit.start_byte);
    }
    let next_cursor = if wire.has_more {
        hit_keys.last().map(|key| Cursor::encode_fjall(key))
    } else {
        None
    };
    CallScanPage {
        total: wire.total,
        total_is_partial: wire.total_is_partial,
        hits,
        next_cursor,
        hit_keys,
        hit_starts,
    }
}

pub(super) fn calls_from_wire(wire: Vec<WireCall>) -> Vec<CallRef> {
    wire.into_iter()
        .map(|c| CallRef {
            callee: c.callee,
            start_byte: c.start_byte,
            line: c.line,
            column: c.column,
        })
        .collect()
}

fn impl_page_to_wire(page: ImplScanPage) -> WireImplScan {
    let hits = page
        .hits
        .into_iter()
        .zip(page.hit_keys)
        .map(|(hit, key)| WireImplHit {
            key,
            path: hit.path,
            trait_name: hit.trait_name,
            impl_type: hit.impl_type,
            start_row: hit.start_row,
            start_col: hit.start_col,
        })
        .collect();
    WireImplScan {
        total: page.total as u64,
        total_is_partial: page.total_is_partial,
        has_more: page.has_more,
        hits,
    }
}

pub(super) fn impl_page_from_wire(wire: WireImplScan) -> ImplScanPage {
    let mut hits = Vec::with_capacity(wire.hits.len());
    let mut hit_keys = Vec::with_capacity(wire.hits.len());
    for hit in wire.hits {
        hits.push(ImplementationHit {
            path: hit.path,
            trait_name: hit.trait_name,
            impl_type: hit.impl_type,
            start_row: hit.start_row,
            start_col: hit.start_col,
        });
        hit_keys.push(hit.key);
    }
    ImplScanPage {
        total: usize::try_from(wire.total).unwrap_or(usize::MAX),
        total_is_partial: wire.total_is_partial,
        hits,
        hit_keys,
        has_more: wire.has_more,
    }
}
