//! Helper body for the `find_implementations` MCP tool.
//!
//! Mirrors the structure of `helpers_calls.rs`: a full-partition scan over
//! `implementations_by_trait` with a `memmem` case-sensitive substring filter,
//! bounded by `scan_cap = limit * 8`, with optional language filtering via the
//! in-RAM `MapCache`.

use std::ops::Bound;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::cursor::Cursor;
use super::helpers::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX, json_result};
use super::types_impls::{
    FindImplementationsParams, FindImplementationsResponse, ImplementationHit,
};

/// Body of the `find_implementations` MCP tool. Performs a full-partition scan over the
/// `implementations_by_trait` Fjall partition with a case-sensitive substring filter on
/// `trait_name`, returning up to `limit` hits with optional language filtering.
pub(super) fn run_find_implementations(
    idx: Option<&crate::index::IndexDb>,
    params: FindImplementationsParams,
    cache: &super::MapCache,
) -> Result<CallToolResult, McpError> {
    let limit = params
        .limit
        .unwrap_or(SEARCH_LIMIT_DEFAULT)
        .min(SEARCH_LIMIT_MAX) as usize;

    let Some(idx) = idx else {
        return json_result(&FindImplementationsResponse {
            trait_name: params.trait_name,
            total: 0,
            total_is_partial: false,
            hits: Vec::new(),
            next_cursor: None,
        });
    };

    let cursor_bytes = params
        .cursor
        .as_ref()
        .map(|c| c.decode_fjall())
        .transpose()?;

    // Build the finder once for the full-partition substring scan.
    let finder = memchr::memmem::Finder::new(params.trait_name.as_bytes());

    let lower: Bound<Vec<u8>> = match cursor_bytes.as_deref() {
        Some(k) => Bound::Excluded(k.to_vec()),
        None => Bound::Unbounded,
    };

    let scan_cap = limit.saturating_mul(8).max(2_000);
    let mut hits: Vec<ImplementationHit> = Vec::with_capacity(limit.min(64));
    let mut total: usize = 0;
    let mut total_is_partial = false;
    let mut last_emitted_key: Option<Vec<u8>> = None;
    let mut has_more = false;
    let mut matched: usize = 0;

    for guard in idx
        .implementations_by_trait
        .range::<Vec<u8>, _>((lower, Bound::Unbounded))
    {
        let (k, _) = guard
            .into_inner()
            .map_err(|e| McpError::internal_error(format!("impl index iter: {e}"), None))?;

        let Some((trait_name, impl_type, rel, start_byte)) =
            crate::index::keys::parse_impl_by_trait(&k)
        else {
            continue;
        };

        // Case-sensitive substring filter on trait_name.
        if finder.find(trait_name.as_bytes()).is_none() {
            continue;
        }

        // Language filter: look up the file's L1 blob in the in-RAM cache.
        // Applied after the substring filter — filtered entries don't count toward scan_cap.
        if let Some(lang_filter) = params.language.as_deref() {
            let l1_lang = cache.by_path.get(&rel).map(|l1| l1.language.as_str());
            if l1_lang != Some(lang_filter) {
                continue;
            }
        }

        total += 1;
        matched += 1;

        if hits.len() < limit {
            // Resolve start_row / start_col from the stored Implementation in the L1 blob.
            let (start_row, start_col) =
                resolve_impl_row_col(cache, &rel, &trait_name, &impl_type, start_byte);
            hits.push(ImplementationHit {
                path: rel,
                trait_name,
                impl_type,
                start_row,
                start_col,
            });
            last_emitted_key = Some(k.to_vec());
        } else {
            has_more = true;
        }

        if matched >= scan_cap {
            total_is_partial = true;
            break;
        }
    }

    let next_cursor = if has_more {
        last_emitted_key.as_deref().map(Cursor::encode_fjall)
    } else {
        None
    };

    json_result(&FindImplementationsResponse {
        trait_name: params.trait_name,
        total,
        total_is_partial,
        hits,
        next_cursor,
    })
}

/// Look up `(start_row, start_col)` for an `Implementation` record from the in-RAM L1
/// cache. Falls back to `(0, 0)` when the cache entry is absent or the matching
/// `Implementation` record isn't found (e.g. an older blob predating the field).
fn resolve_impl_row_col(
    cache: &super::MapCache,
    rel: &crate::path::RelPath,
    trait_name: &str,
    impl_type: &str,
    start_byte: u32,
) -> (u32, u32) {
    let Some(l1) = cache.by_path.get(rel) else {
        return (0, 0);
    };
    // Match by all three discriminants: (trait_name, impl_type, start_byte) is a stable
    // compound key — the same triple is what the index writer encoded.
    if let Some(imp) = l1.implementations.iter().find(|i| {
        i.trait_name == trait_name && i.impl_type == impl_type && i.start_byte == start_byte
    }) {
        // start_row is 0-based in the blob; emit 1-based for line numbers per editor convention.
        (imp.start_row + 1, imp.start_col)
    } else {
        (0, 0)
    }
}
