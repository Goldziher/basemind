//! Body of `code_refs` relation `implementations`.
//!
//! Mirrors the structure of `calls.rs`: a full-partition scan over
//! `implementations_by_trait` with a `memmem` case-sensitive substring filter,
//! bounded by `scan_cap = limit * 8`, with optional language filtering, over the session's index.

use std::ops::Bound;

use super::calls_scan::SCAN_KEYS_EXAMINED_MAX;
use super::matching::Matching;
use crate::engine::MapCache;
use crate::engine::cursor::Cursor;
use crate::engine::error::EngineError;
use crate::engine::ops::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX};
use crate::engine::types::LifecycleNotice;
use crate::engine::types::refs::{FindImplementationsParams, FindImplementationsResponse, ImplementationHit};
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
    /// The last key examined, when the scan stopped at
    /// [`SCAN_KEYS_EXAMINED_MAX`](super::calls_scan::SCAN_KEYS_EXAMINED_MAX) with room left
    /// on the page; the cursor is then this key. Never set together with `has_more`.
    pub stopped_after: Option<Vec<u8>>,
}

/// Body of `code_refs` relation `implementations`. Scans `implementations_by_trait` with a
/// case-sensitive substring filter on `trait_name`, returning up to `limit` hits with optional
/// language filtering, then applies the token budget.
pub(crate) fn run_find_implementations(
    idx: &dyn crate::index::view::IndexView,
    params: FindImplementationsParams,
    cache: &MapCache,
    notice: impl FnOnce() -> Option<LifecycleNotice>,
) -> Result<FindImplementationsResponse, EngineError> {
    let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    let cursor_bytes = params.cursor.as_ref().map(|c| c.decode_fjall()).transpose()?;
    let matching = Matching::from_substring(params.substring);
    let page = scan_impls_session(
        idx,
        cache,
        &params.trait_name,
        matching,
        params.language.as_deref(),
        limit,
        cursor_bytes.as_deref(),
    )?;
    let ImplScanPage {
        total,
        total_is_partial,
        hits,
        hit_keys,
        has_more,
        stopped_after,
    } = page;
    let next_cursor = if has_more {
        hit_keys.last().map(|k| Cursor::encode_fjall(k))
    } else {
        stopped_after.as_deref().map(Cursor::encode_fjall)
    };
    let hits: Vec<_> = hits.into_iter().map(|hit| hit.named(&params.trait_name)).collect();
    let budget = crate::engine::budget::apply_budget_by_path(hits, params.max_tokens);
    let (hits, budgeted, next_cursor) = if budget.budgeted {
        let kept = budget.items.len();
        let cursor = hit_keys.get(kept - 1).map(|k| Cursor::encode_fjall(k));
        (budget.items, true, cursor)
    } else {
        (budget.items, false, next_cursor)
    };

    Ok(FindImplementationsResponse {
        total,
        truncated: total_is_partial,
        truncated_reason: total_is_partial.then_some(crate::engine::types::SCAN_CAP),
        budgeted,
        hits,
        next_cursor,
        notice: notice(),
    })
}

/// The Fjall `implementations_by_trait` scan. The two environment lookups it needs — whether a file
/// is of the requested language, and a hit's `(row, col)` — are injected, because the session
/// answers them from its in-RAM map while the daemon answers them from its store, and both must
/// run this exact loop.
#[allow(
    clippy::too_many_arguments,
    reason = "one scan loop shared by the session and the daemon, which differ in the two injected lookups"
)]
pub(crate) fn scan_impls_fjall(
    idx: &dyn crate::index::view::IndexView,
    trait_name: &str,
    matching: Matching,
    language: Option<&str>,
    limit: usize,
    cursor_after: Option<&[u8]>,
    language_matches: impl Fn(&RelPath, &str) -> bool,
    row_col: impl Fn(&RelPath, u32) -> (u32, u32),
) -> Result<ImplScanPage, EngineError> {
    scan_impls_examining(
        idx,
        trait_name,
        matching,
        language,
        limit,
        cursor_after,
        language_matches,
        row_col,
        SCAN_KEYS_EXAMINED_MAX,
    )
}

/// [`scan_impls_fjall`] with the examined-keys cap as an argument, so a test can reach it.
#[allow(clippy::too_many_arguments, reason = "scan_impls_fjall plus the cap it fixes")]
pub(crate) fn scan_impls_examining(
    idx: &dyn crate::index::view::IndexView,
    trait_name: &str,
    matching: Matching,
    language: Option<&str>,
    limit: usize,
    cursor_after: Option<&[u8]>,
    language_matches: impl Fn(&RelPath, &str) -> bool,
    row_col: impl Fn(&RelPath, u32) -> (u32, u32),
    examine_cap: usize,
) -> Result<ImplScanPage, EngineError> {
    let examine_cap = examine_cap.max(1);
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

    let wanted = trait_name;
    let mut last_examined: Option<crate::index::view::IndexBytes> = None;
    let mut stopped_after: Option<Vec<u8>> = None;
    for (examined, guard) in idx.trait_keys_matching(trait_name, cursor_after, lower).enumerate() {
        let (k, _) = guard.map_err(|e| EngineError::internal(format!("impl index iter: {e}")))?;
        // Checked on the key that follows the cap, so a scan that ends exactly there is complete.
        if examined >= examine_cap {
            total_is_partial = true;
            stopped_after = last_examined.take().map(|key| key.to_vec());
            break;
        }

        let accepted = crate::index::keys::parse_impl_by_trait(&k).filter(|(trait_name, _, rel, _)| {
            finder.find(trait_name.as_bytes()).is_some()
                && matching.accepts_trait(trait_name, wanted)
                && language.is_none_or(|lang_filter| language_matches(rel, lang_filter))
        });
        let Some((trait_name, impl_type, rel, start_byte)) = accepted else {
            last_examined = Some(k);
            continue;
        };

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
        last_examined = Some(k);
    }
    Ok(ImplScanPage {
        total,
        total_is_partial,
        hits,
        hit_keys,
        has_more,
        stopped_after: if has_more { None } else { stopped_after },
    })
}

/// [`scan_impls_fjall`] with the session's environment: language from the in-RAM file view and
/// `(row, col)` from the cached L1 outline.
pub(crate) fn scan_impls_session(
    idx: &dyn crate::index::view::IndexView,
    cache: &MapCache,
    trait_name: &str,
    matching: Matching,
    language: Option<&str>,
    limit: usize,
    cursor_after: Option<&[u8]>,
) -> Result<ImplScanPage, EngineError> {
    scan_impls_fjall(
        idx,
        trait_name,
        matching,
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
fn resolve_impl_row_col(cache: &MapCache, rel: &RelPath, start_byte: u32) -> (u32, u32) {
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
