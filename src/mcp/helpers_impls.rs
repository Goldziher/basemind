//! Helper body for the `find_implementations` MCP tool.
//!
//! Mirrors the structure of `helpers_calls.rs`: a full-partition scan over
//! `implementations_by_trait` with a `memmem` case-sensitive substring filter,
//! bounded by `scan_cap = limit * 8`, with optional language filtering. WHERE the scan runs is
//! [`IndexRoute`]'s decision: this session's fjall index, the machine daemon's (forwarded), or an
//! in-RAM projection when neither is reachable.

use std::ops::Bound;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::cursor::Cursor;
use super::helpers::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX, elapsed_us, json_result};
use super::index_route::IndexRoute;
use super::types_impls::{FindImplementationsParams, FindImplementationsResponse, ImplementationHit};
use crate::path::RelPath;

/// One page of the implementation scan, before the token budget is applied.
pub(crate) struct ImplScanPage {
    pub total: usize,
    pub total_is_partial: bool,
    pub hits: Vec<ImplementationHit>,
    /// Parallel to `hits`: each hit's index key, so a token budget can re-anchor the cursor.
    pub hit_keys: Vec<Vec<u8>>,
    /// Whether matches remain past this page; the cursor is the last hit's key.
    pub has_more: bool,
}

/// Body of the `code` tool's `implementations` mode. Scans `implementations_by_trait` with a
/// case-sensitive substring filter on `trait_name`, returning up to `limit` hits with optional
/// language filtering, then applies the token budget.
pub(super) async fn run_find_implementations(
    route: &IndexRoute,
    params: FindImplementationsParams,
    cache: &super::MapCache,
    notice: impl FnOnce() -> Option<super::types::LifecycleNotice>,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    let cursor_bytes = params.cursor.as_ref().map(|c| c.decode_fjall()).transpose()?;
    let page = route
        .scan_impls(
            cache,
            &params.trait_name,
            params.language.as_deref(),
            limit,
            cursor_bytes.as_deref(),
        )
        .await?;
    let ImplScanPage {
        total,
        total_is_partial,
        hits,
        hit_keys,
        has_more,
    } = page;
    let next_cursor = if has_more {
        hit_keys.last().map(|k| Cursor::encode_fjall(k))
    } else {
        None
    };
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
        notice: notice(),
        elapsed_us: elapsed_us(started),
    })
}

/// The Fjall `implementations_by_trait` scan. The two environment lookups it needs — whether a file
/// is of the requested language, and a hit's `(row, col)` — are injected, because the session
/// answers them from its in-RAM map while the daemon answers them from its store, and both must
/// run this exact loop.
pub(crate) fn scan_impls_fjall(
    idx: &crate::index::IndexDb,
    trait_name: &str,
    language: Option<&str>,
    limit: usize,
    cursor_after: Option<&[u8]>,
    language_matches: impl Fn(&RelPath, &str) -> bool,
    row_col: impl Fn(&RelPath, u32) -> (u32, u32),
) -> Result<ImplScanPage, McpError> {
    let finder = memchr::memmem::Finder::new(trait_name.as_bytes());
    let lower: Bound<Vec<u8>> = match cursor_after {
        Some(k) => Bound::Excluded(k.to_vec()),
        None => Bound::Unbounded,
    };
    let scan_cap = limit.saturating_mul(8).max(2_000);
    let mut hits: Vec<ImplementationHit> = Vec::with_capacity(limit.min(64));
    let mut hit_keys: Vec<Vec<u8>> = Vec::with_capacity(limit.min(64));
    let mut total: usize = 0;
    let mut total_is_partial = false;
    let mut has_more = false;
    let mut matched: usize = 0;

    for guard in crate::index::name_dict::name_ordered_keys(
        &idx.implementations_by_trait,
        &idx.trait_names,
        trait_name,
        cursor_after,
        lower,
    ) {
        let (k, _) = guard
            .into_inner()
            .map_err(|e| McpError::internal_error(format!("impl index iter: {e}"), None))?;

        let Some((trait_name, impl_type, rel, start_byte)) = crate::index::keys::parse_impl_by_trait(&k) else {
            continue;
        };

        if finder.find(trait_name.as_bytes()).is_none() {
            continue;
        }

        if let Some(lang_filter) = language
            && !language_matches(&rel, lang_filter)
        {
            continue;
        }

        total += 1;
        matched += 1;

        if hits.len() < limit {
            let (start_row, start_col) = row_col(&rel, start_byte);
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
    Ok(ImplScanPage {
        total,
        total_is_partial,
        hits,
        hit_keys,
        has_more,
    })
}

/// [`scan_impls_fjall`] with the session's environment: language from the in-RAM file view and
/// `(row, col)` from the cached L1 outline.
pub(crate) fn scan_impls_session(
    idx: &crate::index::IndexDb,
    cache: &super::MapCache,
    trait_name: &str,
    language: Option<&str>,
    limit: usize,
    cursor_after: Option<&[u8]>,
) -> Result<ImplScanPage, McpError> {
    scan_impls_fjall(
        idx,
        trait_name,
        language,
        limit,
        cursor_after,
        |rel, lang| cache.language_of(rel) == Some(lang),
        |rel, start_byte| resolve_impl_row_col(cache, rel, start_byte),
    )
}

/// Look up `(start_row, start_col)` for an `Implementation` record from the in-RAM L1
/// cache. Falls back to `(0, 0)` when the cache entry is absent or the matching
/// `Implementation` record isn't found (e.g. an older blob predating the field).
///
/// `start_byte` is the sole discriminant: an impl/class block has a unique byte offset
/// within a file, so no additional fields are needed.
fn resolve_impl_row_col(cache: &super::MapCache, rel: &RelPath, start_byte: u32) -> (u32, u32) {
    cache.get(rel).map_or((0, 0), |l1| impl_row_col(&l1, start_byte))
}

/// `(start_row, start_col)` of the implementation at `start_byte` in an already-decoded outline,
/// or `(0, 0)` when it has no such record. Shared by the session (cached L1) and the daemon
/// (blob read) so the two report identical positions.
pub(crate) fn impl_row_col(l1: &crate::extract::FileMapL1, start_byte: u32) -> (u32, u32) {
    l1.implementations
        .iter()
        .find(|i| i.start_byte == start_byte)
        .map_or((0, 0), |imp| (imp.start_row + 1, imp.start_col))
}

/// In-RAM twin of [`scan_impls_session`] over the [`InRamImplIndex`] projection. Mirrors the Fjall
/// scan's substring filter, language filter, `scan_cap` and cursor semantics.
pub(crate) fn scan_impls_in_ram(
    index: &InRamImplIndex,
    cache: &super::MapCache,
    trait_name: &str,
    language: Option<&str>,
    limit: usize,
    cursor_after: Option<&[u8]>,
) -> ImplScanPage {
    let finder = memchr::memmem::Finder::new(trait_name.as_bytes());
    let start = match cursor_after {
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
        if let Some(lang) = language
            && cache.language_of(&entry.rel) != Some(lang)
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
    ImplScanPage {
        total,
        total_is_partial,
        hits,
        hit_keys,
        has_more,
    }
}

/// In-RAM mirror of the Fjall `implementations_by_trait` keyspace, projected from every file's
/// L1 `implementations`. Built lazily, and only when a session has neither a fjall index nor a
/// reachable daemon to forward to; keys reuse `keys::impl_by_trait` so cursors round-trip with the
/// Fjall path.
pub(crate) struct InRamImplIndex {
    /// Sorted ascending by key to match Fjall's `range` iteration order.
    entries: Vec<InRamImpl>,
    /// True when the byte budget cut the projection short, so its answers are incomplete.
    capped: bool,
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
    /// Project every file's implementation records into the sorted index, streaming the corpus so
    /// no whole-corpus set of decoded L1s is ever live, and stopping once the projection itself
    /// would exceed `budget_bytes` (`0` = unbounded).
    ///
    /// The cap is what keeps a read-only session — the NORMAL front-end topology under
    /// daemon-writer, where the store has no Fjall index — from holding an O(corpus) structure for
    /// the process lifetime. Truncation is reported, never silent: see
    /// [`capped`](Self::capped) and `LifecycleNotice::projections_capped`.
    pub(crate) fn build(
        files: &super::l1_cache::FileIndexView,
        l1: &super::l1_cache::L1Cache,
        budget_bytes: u64,
    ) -> Self {
        let mut entries: Vec<InRamImpl> = Vec::new();
        let mut charged: u64 = 0;
        let mut capped = false;
        super::l1_cache::stream_while(files, l1, |rel, map| {
            for imp in &map.implementations {
                let Some(key) = crate::index::keys::impl_by_trait(&imp.trait_name, &imp.impl_type, rel, imp.start_byte)
                else {
                    continue;
                };
                charged += (key.len() + imp.trait_name.len() + imp.impl_type.len() + rel.as_bytes().len()) as u64
                    + std::mem::size_of::<InRamImpl>() as u64;
                if budget_bytes != 0 && charged > budget_bytes {
                    capped = true;
                    return false;
                }
                entries.push(InRamImpl {
                    key,
                    trait_name: imp.trait_name.clone(),
                    impl_type: imp.impl_type.clone(),
                    rel: rel.clone(),
                    start_row: imp.start_row + 1,
                    start_col: imp.start_col,
                });
            }
            true
        });
        entries.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        Self { entries, capped }
    }

    /// Whether the byte budget truncated this projection.
    pub(crate) fn capped(&self) -> bool {
        self.capped
    }
}
