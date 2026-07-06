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
use super::types_impls::{FindImplementationsParams, FindImplementationsResponse, ImplementationHit};

/// Body of the `find_implementations` MCP tool. Performs a full-partition scan over the
/// `implementations_by_trait` Fjall partition with a case-sensitive substring filter on
/// `trait_name`, returning up to `limit` hits with optional language filtering.
pub(super) fn run_find_implementations(
    idx: Option<&crate::index::IndexDb>,
    params: FindImplementationsParams,
    cache: &super::MapCache,
) -> Result<CallToolResult, McpError> {
    let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;

    // No Fjall index (read-only session that lost the single-holder lock): answer
    // from the in-RAM impl index built off the L1 blobs.
    let Some(idx) = idx else {
        return find_implementations_in_ram(cache, params, limit);
    };

    let cursor_bytes = params.cursor.as_ref().map(|c| c.decode_fjall()).transpose()?;

    // Build the finder once for the full-partition substring scan.
    let finder = memchr::memmem::Finder::new(params.trait_name.as_bytes());

    let lower: Bound<Vec<u8>> = match cursor_bytes.as_deref() {
        Some(k) => Bound::Excluded(k.to_vec()),
        None => Bound::Unbounded,
    };

    let scan_cap = limit.saturating_mul(8).max(2_000);
    let mut hits: Vec<ImplementationHit> = Vec::with_capacity(limit.min(64));
    // Parallel to `hits`: the Fjall key for each emitted hit, so a token budget can re-anchor
    // the cursor to the last KEPT hit instead of the last scanned one.
    let mut hit_keys: Vec<Vec<u8>> = Vec::with_capacity(limit.min(64));
    let mut total: usize = 0;
    let mut total_is_partial = false;
    let mut has_more = false;
    let mut matched: usize = 0;

    for guard in idx
        .implementations_by_trait
        .range::<Vec<u8>, _>((lower, Bound::Unbounded))
    {
        let (k, _) = guard
            .into_inner()
            .map_err(|e| McpError::internal_error(format!("impl index iter: {e}"), None))?;

        let Some((trait_name, impl_type, rel, start_byte)) = crate::index::keys::parse_impl_by_trait(&k) else {
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
            let (start_row, start_col) = resolve_impl_row_col(cache, &rel, start_byte);
            hits.push(ImplementationHit {
                path: rel,
                trait_name,
                impl_type,
                start_row,
                start_col,
            });
            hit_keys.push(k.to_vec());
        } else {
            has_more = true;
        }

        if matched >= scan_cap {
            total_is_partial = true;
            break;
        }
    }

    let next_cursor = if has_more {
        hit_keys.last().map(|k| Cursor::encode_fjall(k))
    } else {
        None
    };

    // Apply the token budget. When it drops trailing hits, re-anchor the cursor to the last
    // KEPT hit's Fjall key so the next page resumes exactly after it. A no-op when
    // `max_tokens` is None or every hit fit.
    let budget = super::budget::apply_budget(hits, params.max_tokens);
    let (hits, budgeted, next_cursor) = if budget.budgeted {
        let kept = budget.items.len();
        let cursor = hit_keys.get(kept - 1).map(|k| Cursor::encode_fjall(k));
        (budget.items, true, cursor)
    } else {
        (budget.items, false, next_cursor)
    };

    json_result(&FindImplementationsResponse {
        trait_name: params.trait_name,
        total,
        total_is_partial,
        budgeted,
        hits,
        next_cursor,
    })
}

/// Look up `(start_row, start_col)` for an `Implementation` record from the in-RAM L1
/// cache. Falls back to `(0, 0)` when the cache entry is absent or the matching
/// `Implementation` record isn't found (e.g. an older blob predating the field).
///
/// `start_byte` is the sole discriminant: an impl/class block has a unique byte offset
/// within a file, so no additional fields are needed.
fn resolve_impl_row_col(cache: &super::MapCache, rel: &crate::path::RelPath, start_byte: u32) -> (u32, u32) {
    let Some(l1) = cache.by_path.get(rel) else {
        return (0, 0);
    };
    // Match by start_byte alone: an `impl` / class definition has a unique byte offset
    // within a file, so start_byte is a sufficient key into l1.implementations. The
    // index writer encodes exactly one record per (path, start_byte) pair, making
    // trait_name and impl_type redundant discriminants here.
    if let Some(imp) = l1.implementations.iter().find(|i| i.start_byte == start_byte) {
        // start_row is 0-based in the blob; emit 1-based for line numbers per editor convention.
        (imp.start_row + 1, imp.start_col)
    } else {
        (0, 0)
    }
}

/// In-RAM `find_implementations` for read-only sessions (no Fjall): scans the
/// [`InRamImplIndex`] built from the L1 blobs. Mirrors the Fjall scan's substring
/// filter, language filter, `scan_cap`, cursor, and token-budget semantics.
fn find_implementations_in_ram(
    cache: &super::MapCache,
    params: FindImplementationsParams,
    limit: usize,
) -> Result<CallToolResult, McpError> {
    let Some(index) = cache.impls.as_ref() else {
        return json_result(&FindImplementationsResponse {
            trait_name: params.trait_name,
            total: 0,
            total_is_partial: false,
            budgeted: false,
            hits: Vec::new(),
            next_cursor: None,
        });
    };
    let cursor_bytes = params.cursor.as_ref().map(|c| c.decode_fjall()).transpose()?;
    let finder = memchr::memmem::Finder::new(params.trait_name.as_bytes());
    // Entries are sorted by key; resume = first entry strictly past the cursor.
    let start = match cursor_bytes.as_deref() {
        Some(c) => index.entries.partition_point(|e| e.key.as_slice() <= c),
        None => 0,
    };
    let scan_cap = limit.saturating_mul(8).max(2_000);
    let mut hits: Vec<ImplementationHit> = Vec::with_capacity(limit.min(64));
    let mut hit_keys: Vec<Vec<u8>> = Vec::with_capacity(limit.min(64));
    let mut total: usize = 0;
    let mut total_is_partial = false;
    let mut has_more = false;
    let mut matched: usize = 0;
    for entry in &index.entries[start..] {
        if finder.find(entry.trait_name.as_bytes()).is_none() {
            continue;
        }
        if let Some(lang) = params.language.as_deref()
            && cache.by_path.get(&entry.rel).map(|l1| l1.language.as_str()) != Some(lang)
        {
            continue;
        }
        total += 1;
        matched += 1;
        if hits.len() < limit {
            hits.push(ImplementationHit {
                path: entry.rel.clone(),
                trait_name: entry.trait_name.clone(),
                impl_type: entry.impl_type.clone(),
                start_row: entry.start_row,
                start_col: entry.start_col,
            });
            hit_keys.push(entry.key.clone());
        } else {
            has_more = true;
        }
        if matched >= scan_cap {
            total_is_partial = true;
            break;
        }
    }
    let next_cursor = if has_more {
        hit_keys.last().map(|k| Cursor::encode_fjall(k))
    } else {
        None
    };
    let budget = super::budget::apply_budget(hits, params.max_tokens);
    let (hits, budgeted, next_cursor) = if budget.budgeted {
        let kept = budget.items.len();
        (
            budget.items,
            true,
            hit_keys.get(kept - 1).map(|k| Cursor::encode_fjall(k)),
        )
    } else {
        (budget.items, false, next_cursor)
    };
    json_result(&FindImplementationsResponse {
        trait_name: params.trait_name,
        total,
        total_is_partial,
        budgeted,
        hits,
        next_cursor,
    })
}

/// In-RAM mirror of the Fjall `implementations_by_trait` keyspace, built from the
/// L1 `implementations` already held by `MapCache::by_path`. Populated only for
/// read-only sessions; keys reuse `keys::impl_by_trait` so cursors round-trip with
/// the Fjall path.
pub(crate) struct InRamImplIndex {
    /// Sorted ascending by key to match Fjall's `range` iteration order.
    entries: Vec<InRamImpl>,
}

struct InRamImpl {
    key: Vec<u8>,
    trait_name: String,
    impl_type: String,
    rel: crate::path::RelPath,
    /// 1-based line (`start_row + 1`), matching `resolve_impl_row_col`.
    start_row: u32,
    /// 0-based byte column.
    start_col: u32,
}

impl InRamImplIndex {
    pub(crate) fn build(by_path: &std::collections::BTreeMap<crate::path::RelPath, crate::extract::FileMapL1>) -> Self {
        let mut entries: Vec<InRamImpl> = Vec::new();
        for (rel, l1) in by_path {
            for imp in &l1.implementations {
                if let Some(key) =
                    crate::index::keys::impl_by_trait(&imp.trait_name, &imp.impl_type, rel, imp.start_byte)
                {
                    entries.push(InRamImpl {
                        key,
                        trait_name: imp.trait_name.clone(),
                        impl_type: imp.impl_type.clone(),
                        rel: rel.clone(),
                        start_row: imp.start_row + 1,
                        start_col: imp.start_col,
                    });
                }
            }
        }
        entries.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        Self { entries }
    }
}
