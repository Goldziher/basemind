//! Call-scan primitives for `find_references` / `find_callers`: the `calls_by_callee`
//! range scan, its token-budget re-anchoring, and the per-file call reads behind them.
//!
//! Extracted from `calls.rs` to keep that file within the per-file size budget.

use std::ops::Bound;

use super::calls::resolve_call_line_col;
use super::matching::Matching;
use crate::engine::cursor::Cursor;
use crate::engine::error::EngineError;
use crate::engine::scope::ChangeScope;
use crate::engine::types::refs::{ReferenceHit, Resolution};
use crate::index::keys::ContentKey;
use crate::index::view::IndexView;
use crate::path::RelPath;

/// A call site within a file: the callee identifier, its start byte offset, and its position.
#[derive(Clone)]
pub(crate) struct CallRef {
    pub callee: String,
    pub start_byte: u32,
    /// 1-based line (`start_row + 1`), matching `resolve_call_line_col`.
    pub line: u32,
    /// 0-based byte column.
    pub column: u32,
}

/// The `calls_by_callee` name scan behind `references` and `callers`, with the generation of the
/// key order it was cut in: the fingerprint of the view's live keys. A `references` / `callers`
/// cursor carries it, and is refused as stale once it changes, since an edit moves the edited
/// file's hits in that order.
/// Call sites in files outside `scope` are not matches: they neither count nor list.
pub(crate) fn scan_calls(
    idx: &dyn IndexView,
    name: &str,
    matching: Matching,
    limit: usize,
    cursor_after: Option<&[u8]>,
    scope: Option<&ChangeScope>,
) -> Result<(CallScanPage, u64), EngineError> {
    let generation = idx.live_keys().fingerprint();
    let finder = memchr::memmem::Finder::new(name.as_bytes());
    let page = scan_calls_where(
        idx,
        name,
        limit,
        cursor_after,
        SCAN_KEYS_EXAMINED_MAX,
        |callee, path| {
            (finder.find(callee.as_bytes()).is_some()
                && matching.accepts_callee(callee, name)
                && crate::engine::changed::admits(scope, path))
            .then_some(Kept::NAME_ONLY)
        },
    )?;
    Ok((page, generation))
}

/// How a scan lists a call site it keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Kept {
    pub(crate) resolution: Resolution,
    pub(crate) ambiguous: bool,
}

impl Kept {
    /// A site matched by name, before any resolution grades it.
    pub(crate) const NAME_ONLY: Self = Self {
        resolution: Resolution::NameOnly,
        ambiguous: false,
    };
}

/// One file's call sites with the content key they were read under.
pub(crate) type FileCalls = (ContentKey, Vec<CallRef>);

/// The content key each of `paths` is read under, in request order, from one snapshot of the view's
/// live keys; `None` for a path the view does not hold.
pub(crate) fn content_keys(idx: &dyn IndexView, paths: &[RelPath]) -> Vec<Option<ContentKey>> {
    let live = idx.live_keys();
    paths.iter().map(|path| live.key_of(path).copied()).collect()
}

/// Every call site of each of `files`, in request order, with the content key it was read under
/// (the key given with it): the order key of the file's calls, which places a resolved caller among
/// the scan hits. `None` for a file read under no key, which has no calls.
pub(crate) fn calls_in_files(
    idx: &dyn IndexView,
    files: &[(RelPath, Option<ContentKey>)],
) -> Result<Vec<Option<FileCalls>>, EngineError> {
    files
        .iter()
        .map(|(_, content)| {
            let Some(content) = content else {
                return Ok(None);
            };
            calls_in_content_fjall(idx, Some(content)).map(|calls| Some((*content, calls)))
        })
        .collect()
}

/// Keys one name scan examines before it stops, whether or not they matched. `scan_cap` counts only
/// accepted hits, so without this an exact scan for a name that is also a common fragment (`get`)
/// walks every key containing the fragment before it returns. A scan that stops here reports
/// `total_is_partial` and resumes from the last key it examined.
pub(crate) const SCAN_KEYS_EXAMINED_MAX: usize = 200_000;

pub(crate) struct CallScanPage {
    pub total: u32,
    pub total_is_partial: bool,
    pub hits: Vec<ReferenceHit>,
    pub next_cursor: Option<Cursor>,
    /// The last key examined, when the scan stopped at its examined-keys cap with room left on the
    /// page. `next_cursor` then encodes this key instead of the last hit's, so the next page
    /// continues the scan where it stopped.
    pub stopped_after: Option<Vec<u8>>,
    /// Parallel to `hits`: the Fjall key for each emitted hit. Retained so a token budget can
    /// re-anchor `next_cursor` to the last KEPT hit, not the last scanned one.
    pub hit_keys: Vec<Vec<u8>>,
    /// Parallel to `hits`: each hit's call-site start byte. `find_callers` probes it against the
    /// resolved call-site set to set `ReferenceHit::resolution` without re-deriving the offset.
    pub hit_starts: Vec<u32>,
}

impl CallScanPage {
    /// Assemble a scan's page. The cursor is the last hit's key when matches remain past the page
    /// (`has_more`), and otherwise the key the scan stopped after, if it stopped early.
    pub(crate) fn finish(
        total: u32,
        total_is_partial: bool,
        hits: Vec<ReferenceHit>,
        hit_keys: Vec<Vec<u8>>,
        hit_starts: Vec<u32>,
        has_more: bool,
        stopped_after: Option<Vec<u8>>,
    ) -> Self {
        let stopped_after = if has_more { None } else { stopped_after };
        let next_cursor = if has_more {
            hit_keys.last().map(|k| Cursor::encode_fjall(k))
        } else {
            stopped_after.as_deref().map(Cursor::encode_fjall)
        };
        Self {
            total,
            total_is_partial,
            hits,
            next_cursor,
            stopped_after,
            hit_keys,
            hit_starts,
        }
    }
}

/// Result of applying a `max_tokens` budget to a call-scan page.
pub(crate) struct BudgetedCallPage {
    pub hits: Vec<ReferenceHit>,
    pub next_cursor: Option<Cursor>,
    pub budgeted: bool,
}

/// Apply a `max_tokens` budget to an already-built call-scan page and recompute its cursor.
///
/// Hits are best-first (scan order). When the budget drops trailing hits the cursor is
/// re-anchored to the last KEPT hit's Fjall key so the next page resumes immediately after
/// it with no gap or overlap. `max_tokens = None` keeps the whole page. Each hit is written as
/// listed for a query of `name` ([`ReferenceHit::named`]) before it is costed, so the budget charges
/// what is written.
///
/// The kept hits are then sorted by path (ADR-0036 rule 6): the scan's key order is by content
/// key, which says nothing to a reader. The sort is stable, so one file's hits stay in offset order.
pub(crate) fn budget_call_page(page: CallScanPage, name: &str, max_tokens: Option<u32>) -> BudgetedCallPage {
    let hits: Vec<ReferenceHit> = page.hits.into_iter().map(|hit| hit.named(name)).collect();
    let mut budgeted = if max_tokens.is_none() {
        BudgetedCallPage {
            hits,
            next_cursor: page.next_cursor,
            budgeted: false,
        }
    } else {
        let budget = crate::engine::budget::apply_budget_by_path(hits, max_tokens);
        if budget.budgeted {
            let kept = budget.items.len();
            BudgetedCallPage {
                hits: budget.items,
                next_cursor: page.hit_keys.get(kept - 1).map(|k| Cursor::encode_fjall(k)),
                budgeted: true,
            }
        } else {
            BudgetedCallPage {
                hits: budget.items,
                next_cursor: page.next_cursor,
                budgeted: false,
            }
        }
    };
    budgeted.hits.sort_by(|a, b| a.path.cmp(&b.path));
    budgeted
}

/// Set [`ReferenceHit::shared_line`] on each hit of a page, in scan order with its `hit_keys`:
/// another call site of the same callee is on its line of its file, anywhere in the result. A scan
/// lists one callee's sites in one content as a run in start-byte order, each site once per file
/// holding the content, so a file whose run lies inside the page holds every such site on the page.
/// Only the files holding the content of the page's first or last hit can have a run cut by the
/// page edge; their call sites are read whole.
pub(crate) fn mark_shared_lines(idx: &dyn IndexView, hits: &mut [ReferenceHit], hit_keys: &[Vec<u8>]) {
    let content = |key: Option<&Vec<u8>>| key.and_then(|key| crate::index::keys::call_by_callee_content(key));
    let edge_contents = [content(hit_keys.first()), content(hit_keys.last())];
    let mut edges: Vec<(crate::path::RelPath, Option<crate::index::keys::ContentKey>)> = Vec::new();
    for (index, hit) in hits.iter().enumerate() {
        let key = content(hit_keys.get(index));
        let at_edge =
            index == 0 || index + 1 == hits.len() || key.is_some_and(|key| edge_contents.contains(&Some(key)));
        if at_edge && !edges.iter().any(|(path, _)| *path == hit.path) {
            edges.push((hit.path.clone(), key));
        }
    }
    let edge_calls: Vec<Vec<CallRef>> = calls_in_files(idx, &edges)
        .unwrap_or_default()
        .into_iter()
        .map(|read| read.map(|(_, calls)| calls).unwrap_or_default())
        .collect();
    let edges: Vec<crate::path::RelPath> = edges.into_iter().map(|(path, _)| path).collect();
    let flags: Vec<bool> = {
        let mut on_page: ahash::AHashMap<(&crate::path::RelPath, u32, &str), usize> = ahash::AHashMap::new();
        for hit in hits.iter() {
            *on_page.entry((&hit.path, hit.line, hit.callee.as_str())).or_default() += 1;
        }
        let mut in_edge: ahash::AHashMap<(&crate::path::RelPath, u32, &str), usize> = ahash::AHashMap::new();
        for (path, calls) in edges.iter().zip(&edge_calls) {
            for call in calls {
                *in_edge.entry((path, call.line, call.callee.as_str())).or_default() += 1;
            }
        }
        hits.iter()
            .map(|hit| {
                let site = (&hit.path, hit.line, hit.callee.as_str());
                let count = if edges.contains(&hit.path) && edge_calls.len() == edges.len() {
                    in_edge.get(&site)
                } else {
                    on_page.get(&site)
                };
                hit.line != 0 && count.is_some_and(|count| *count > 1)
            })
            .collect()
    };
    for (hit, shared) in hits.iter_mut().zip(flags) {
        hit.shared_line = shared;
    }
}

/// Shared inner loop for `find_references` / `find_callers`: full-partition scan of
/// `calls_by_callee` with a `memmem` case-sensitive substring filter on the callee name.
/// Materializes up to `limit` hits and caps at `scan_cap = limit * 8` matching entries
/// to bound work on extremely common names.
///
/// `matching` is applied here, on the key, before a hit's position is resolved and before it counts
/// toward `scan_cap`: under [`Matching::Exact`] a callee that merely contains `name` costs one
/// suffix test, so a common fragment (`get` inside `get_or_insert`) cannot use up the cap and hide
/// the exact call sites that sort after it.
///
/// Rejected keys still cost a parse and a test each, so the scan also stops once it has examined
/// [`SCAN_KEYS_EXAMINED_MAX`] keys with more to go; see [`CallScanPage::stopped_after`].
///
/// When `cursor_after` is `Some`, the scan resumes from the key immediately following
/// the cursor (exclusive). The cursor returned in [`CallScanPage::next_cursor`] is the
/// last key emitted on this page — pass it back on the next call to advance.
#[cfg(test)]
pub(crate) fn scan_calls_by_name(
    idx: &dyn crate::index::view::IndexView,
    name: &str,
    matching: Matching,
    limit: usize,
    cursor_after: Option<&[u8]>,
) -> Result<CallScanPage, EngineError> {
    scan_calls_examining(idx, name, matching, limit, cursor_after, SCAN_KEYS_EXAMINED_MAX)
}

/// [`scan_calls_by_name`] with the examined-keys cap as an argument, so a test can reach it.
#[cfg(test)]
pub(crate) fn scan_calls_examining(
    idx: &dyn crate::index::view::IndexView,
    name: &str,
    matching: Matching,
    limit: usize,
    cursor_after: Option<&[u8]>,
    examine_cap: usize,
) -> Result<CallScanPage, EngineError> {
    let finder = memchr::memmem::Finder::new(name.as_bytes());
    scan_calls_where(idx, name, limit, cursor_after, examine_cap, |callee, _| {
        (finder.find(callee.as_bytes()).is_some() && matching.accepts_callee(callee, name)).then_some(Kept::NAME_ONLY)
    })
}

/// The scan behind [`scan_calls_examining`], over the callees that may contain `needle`: `keep`
/// decides from a callee and its file whether the site is a match and how it is listed. A site it
/// refuses costs no position lookup and does not count toward the scan cap.
pub(crate) fn scan_calls_where(
    idx: &dyn crate::index::view::IndexView,
    needle: &str,
    limit: usize,
    cursor_after: Option<&[u8]>,
    examine_cap: usize,
    mut keep: impl FnMut(&str, &RelPath) -> Option<Kept>,
) -> Result<CallScanPage, EngineError> {
    // A page that examined nothing has no key to resume from.
    let examine_cap = examine_cap.max(1);

    let lower: Bound<Vec<u8>> = match cursor_after {
        Some(k) => Bound::Excluded(k.to_vec()),
        None => Bound::Unbounded,
    };
    let mut hits: Vec<ReferenceHit> = Vec::with_capacity(limit.min(64));
    let mut hit_keys: Vec<Vec<u8>> = Vec::with_capacity(limit.min(64));
    let mut hit_starts: Vec<u32> = Vec::with_capacity(limit.min(64));
    let mut total: u32 = 0;
    let mut total_is_partial = false;
    let scan_cap = limit.saturating_mul(8).max(2_000);
    let mut has_more = false;
    let mut matched: usize = 0;
    let mut last_examined: Option<crate::index::view::IndexBytes> = None;
    let mut stopped_after: Option<Vec<u8>> = None;
    for (examined, guard) in idx.callee_keys_matching(needle, cursor_after, lower).enumerate() {
        let (k, _) = guard.map_err(|e| EngineError::internal(format!("index iter: {e}")))?;
        // Checked on the key that follows the cap, so a scan that ends exactly there is complete.
        if examined >= examine_cap {
            total_is_partial = true;
            stopped_after = last_examined.take().map(|key| key.to_vec());
            break;
        }
        let Some((callee, rel, start)) = crate::index::keys::parse_call_by_callee(&k) else {
            last_examined = Some(k);
            continue;
        };
        let Some(kept) = keep(&callee, &rel) else {
            last_examined = Some(k);
            continue;
        };
        total += 1;
        matched += 1;
        if hits.len() < limit {
            let (line, column) = crate::index::keys::call_by_callee_content(&k)
                .map_or((0, 0), |content| resolve_call_line_col(idx, &content, start));
            hits.push(ReferenceHit {
                path: rel,
                line,
                column,
                callee,
                resolution: kept.resolution,
                elide_resolution: false,
                shared_line: false,
                stale: false,
                ambiguous: kept.ambiguous,
            });
            hit_keys.push(k.to_vec());
            hit_starts.push(start);
        } else {
            has_more = true;
        }
        if matched >= scan_cap {
            total_is_partial = true;
            break;
        }
        last_examined = Some(k);
    }
    Ok(CallScanPage::finish(
        total,
        total_is_partial,
        hits,
        hit_keys,
        hit_starts,
        has_more,
        stopped_after,
    ))
}

/// Every call site of `content` from the Fjall `calls_by_path` keyspace, in start-byte order; none
/// for `None`, a path the view does not hold. The one reader behind the writer session's per-file
/// lookups and the daemon's forwarded `CallsInFiles`, so the two cannot drift. The caller takes the
/// content key from a hit key, or from one live-key snapshot for its whole query.
pub(crate) fn calls_in_content_fjall(
    idx: &dyn crate::index::view::IndexView,
    content: Option<&crate::index::keys::ContentKey>,
) -> Result<Vec<CallRef>, EngineError> {
    let Some(content) = content else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for guard in idx.calls_in_content(content) {
        let (_, v) = guard.map_err(|e| EngineError::internal(format!("index iter: {e}")))?;
        let call: crate::extract::Call = match rmp_serde::from_slice(&v) {
            Ok(c) => c,
            Err(_) => continue,
        };
        out.push(CallRef {
            line: call.start_row + 1,
            column: call.start_col,
            callee: call.callee,
            start_byte: call.start_byte,
        });
    }
    Ok(out)
}

#[cfg(test)]
#[path = "calls_scan_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "calls_group_tests.rs"]
mod group_tests;
