//! Call-scan primitives for `find_references` / `find_callers`: the `calls_by_callee`
//! range scan, its token-budget re-anchoring, and the in-RAM call index built from L2
//! blobs for read-only sessions that can't open the single-holder Fjall index.
//!
//! Extracted from `helpers_calls.rs` to keep that file within the per-file size budget.

use std::ops::Bound;

use rmcp::ErrorData as McpError;

use super::cursor::Cursor;
use super::helpers_calls::resolve_call_line_col;
use super::helpers_refs_match::Matching;
use super::types::ReferenceHit;

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
    /// resolved call-site set to annotate `ReferenceHit::resolved` without re-deriving the offset.
    pub hit_starts: Vec<u32>,
}

impl CallScanPage {
    /// Assemble a scan's page. The cursor is the last hit's key when matches remain past the page
    /// (`has_more`), and otherwise the key the scan stopped after, if it stopped early.
    fn finish(
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

    /// Drop the hits `keep` rejects, with their parallel entries. `total` shrinks by the same count:
    /// it counted every hit the sender matched, of which only this page's share could be tested, so
    /// what is left is an upper bound on the hits `keep` accepts. No scan cap was hit, so
    /// `total_is_partial`, which the response reports as `truncated_reason: "scan_cap"`, is left as the
    /// sender set it.
    #[cfg(any(test, all(feature = "comms", any(unix, windows))))]
    pub(super) fn retain(&mut self, keep: impl Fn(&ReferenceHit) -> bool) {
        let mut index = 0;
        while index < self.hits.len() {
            if keep(&self.hits[index]) {
                index += 1;
                continue;
            }
            self.hits.remove(index);
            self.hit_keys.remove(index);
            self.hit_starts.remove(index);
            self.total = self.total.saturating_sub(1);
        }
    }
}

/// Result of applying a `max_tokens` budget to a call-scan page.
pub(super) struct BudgetedCallPage {
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
pub(super) fn budget_call_page(page: CallScanPage, name: &str, max_tokens: Option<u32>) -> BudgetedCallPage {
    let hits: Vec<ReferenceHit> = page.hits.into_iter().map(|hit| hit.named(name)).collect();
    let mut budgeted = if max_tokens.is_none() {
        BudgetedCallPage {
            hits,
            next_cursor: page.next_cursor,
            budgeted: false,
        }
    } else {
        let budget = super::budget::apply_budget_by_path(hits, max_tokens);
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
pub(super) async fn mark_shared_lines(
    route: &super::index_route::IndexRoute,
    cache: &super::MapCache,
    hits: &mut [ReferenceHit],
    hit_keys: &[Vec<u8>],
) {
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
    let edge_calls: Vec<Vec<CallRef>> = route
        .calls_in_files(cache, &edges)
        .await
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
pub(crate) fn scan_calls_by_name(
    idx: &dyn crate::index::view::IndexView,
    name: &str,
    matching: Matching,
    limit: usize,
    cursor_after: Option<&[u8]>,
) -> Result<CallScanPage, McpError> {
    scan_calls_examining(idx, name, matching, limit, cursor_after, SCAN_KEYS_EXAMINED_MAX)
}

/// [`scan_calls_by_name`] with the examined-keys cap as an argument, so a test can reach it.
pub(super) fn scan_calls_examining(
    idx: &dyn crate::index::view::IndexView,
    name: &str,
    matching: Matching,
    limit: usize,
    cursor_after: Option<&[u8]>,
    examine_cap: usize,
) -> Result<CallScanPage, McpError> {
    // A page that examined nothing has no key to resume from.
    let examine_cap = examine_cap.max(1);
    let finder = memchr::memmem::Finder::new(name.as_bytes());

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
    for (examined, guard) in idx.callee_keys_matching(name, cursor_after, lower).enumerate() {
        let (k, _) = guard.map_err(|e| McpError::internal_error(format!("index iter: {e}"), None))?;
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
        if finder.find(callee.as_bytes()).is_none() || !matching.accepts_callee(&callee, name) {
            last_examined = Some(k);
            continue;
        }
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
                resolved: None,
                shared_line: false,
                stale: false,
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
) -> Result<Vec<CallRef>, McpError> {
    let Some(content) = content else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for guard in idx.calls_in_content(content) {
        let (_, v) = guard.map_err(|e| McpError::internal_error(format!("index iter: {e}"), None))?;
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

/// In-RAM `scan_calls_by_name` twin over [`InRamCallIndex`]. Same case-sensitive
/// `memmem` substring filter, same `matching` / `limit` / `scan_cap` / cursor semantics — the
/// entries carry the exact Fjall key the writer would persist, so cursors and
/// scan order round-trip identically between the two paths.
pub(super) fn scan_calls_in_ram(
    index: &InRamCallIndex,
    name: &str,
    matching: Matching,
    limit: usize,
    cursor_after: Option<&[u8]>,
) -> CallScanPage {
    scan_calls_in_ram_examining(index, name, matching, limit, cursor_after, SCAN_KEYS_EXAMINED_MAX)
}

/// [`scan_calls_in_ram`] with the examined-keys cap as an argument, so a test can reach it.
pub(super) fn scan_calls_in_ram_examining(
    index: &InRamCallIndex,
    name: &str,
    matching: Matching,
    limit: usize,
    cursor_after: Option<&[u8]>,
    examine_cap: usize,
) -> CallScanPage {
    let examine_cap = examine_cap.max(1);
    let finder = memchr::memmem::Finder::new(name.as_bytes());
    let start = match cursor_after {
        Some(cursor) => index.entries.partition_point(|e| e.key.as_slice() <= cursor),
        None => 0,
    };
    let mut hits: Vec<ReferenceHit> = Vec::with_capacity(limit.min(64));
    let mut hit_keys: Vec<Vec<u8>> = Vec::with_capacity(limit.min(64));
    let mut hit_starts: Vec<u32> = Vec::with_capacity(limit.min(64));
    let mut total: u32 = 0;
    let mut total_is_partial = false;
    let scan_cap = limit.saturating_mul(8).max(2_000);
    let mut has_more = false;
    let mut matched: usize = 0;
    let mut stopped_after: Option<Vec<u8>> = None;
    for (examined, entry) in index.entries[start..].iter().enumerate() {
        if examined >= examine_cap {
            total_is_partial = true;
            stopped_after = Some(index.entries[start + examined - 1].key.clone());
            break;
        }
        if finder.find(entry.callee.as_bytes()).is_none() || !matching.accepts_callee(&entry.callee, name) {
            continue;
        }
        total += 1;
        matched += 1;
        if hits.len() < limit {
            hits.push(ReferenceHit {
                path: entry.rel.clone(),
                line: entry.line,
                column: entry.column,
                callee: entry.callee.clone(),
                resolved: None,
                shared_line: false,
                stale: false,
            });
            hit_keys.push(entry.key.clone());
            hit_starts.push(entry.start_byte);
        } else {
            has_more = true;
        }
        if matched >= scan_cap {
            total_is_partial = true;
            break;
        }
    }
    CallScanPage::finish(
        total,
        total_is_partial,
        hits,
        hit_keys,
        hit_starts,
        has_more,
        stopped_after,
    )
}

/// How many files one projection pass decodes before folding them into the views. Bounds the
/// decoded-L2 working set to O(chunk) rather than O(corpus).
const L2_PROJECTION_CHUNK: usize = 256;

/// In-RAM mirror of the Fjall `calls_by_callee` + `calls_by_path` keyspaces, built
/// from the L2 call blobs for read-only `serve` sessions that can't open the
/// single-holder Fjall index. Lets unlimited concurrent sessions answer
/// `find_references` / `find_callers` from the shared, immutable,
/// concurrently-readable blobs.
pub(crate) struct InRamCallIndex {
    /// Sorted ascending by `key` to match Fjall's `range` iteration order
    /// (drives `find_references` / `find_callers`).
    entries: Vec<InRamCall>,
    /// path → its call sites (the `calls_by_path` keyspace).
    by_path: ahash::AHashMap<crate::path::RelPath, Vec<CallRef>>,
    /// True when the byte budget cut the projection short, so its answers are incomplete.
    capped: bool,
}

struct InRamCall {
    /// `keys::call_by_callee_hit(callee, content, start_byte, rel)` — the exact key the index view
    /// yields, reused so cursors round-trip identically across the two paths.
    key: Vec<u8>,
    callee: String,
    rel: crate::path::RelPath,
    /// 0-based byte offset of the call site (for containing-function resolution).
    start_byte: u32,
    /// 1-based line (`start_row + 1`), matching [`resolve_call_line_col`].
    line: u32,
    /// 0-based byte column.
    column: u32,
}

/// A call site within a file: the callee identifier, its start byte offset, and its position.
#[derive(Clone)]
pub(crate) struct CallRef {
    pub callee: String,
    pub start_byte: u32,
    /// 1-based line (`start_row + 1`), matching [`resolve_call_line_col`].
    pub line: u32,
    /// 0-based byte column.
    pub column: u32,
}

impl InRamCallIndex {
    /// Project the L2 calls of every file into the two views, bounded by `budget_bytes`
    /// (`0` = unbounded).
    ///
    /// Reads/decodes run in parallel a chunk at a time; the two views are assembled serially per
    /// chunk. See [`capped`](Self::capped) for what happens when the budget is reached.
    pub(crate) fn build(
        files: &super::l1_cache::FileIndexView,
        blobs_dir: &std::path::Path,
        budget_bytes: u64,
    ) -> Self {
        use rayon::prelude::*;
        let paths: Vec<(&crate::path::RelPath, &str, crate::index::keys::ContentKey)> = files
            .iter()
            .map(|(rel, meta)| (rel, &*meta.hash_hex, meta.order_key()))
            .collect();
        let mut entries: Vec<InRamCall> = Vec::new();
        let mut by_path: ahash::AHashMap<crate::path::RelPath, Vec<CallRef>> =
            ahash::AHashMap::with_capacity(paths.len());
        let mut charged: u64 = 0;
        let mut capped = false;
        // Chunked rather than one whole-corpus `par_iter`: the previous shape materialised EVERY
        // file's decoded L2 calls before assembling either view, so peak residency was the sum of
        // the projection and the raw decode it was projected from. Here at most one chunk of
        // decoded blobs is live, and the loop stops the moment the projection reaches its budget.
        for chunk in paths.chunks(L2_PROJECTION_CHUNK) {
            let per_file: Vec<(
                crate::path::RelPath,
                crate::index::keys::ContentKey,
                Vec<crate::extract::Call>,
            )> = chunk
                .par_iter()
                .filter_map(|(rel, hash_hex, content)| {
                    let calls = crate::store_blob::read_l2_blob_in(blobs_dir, hash_hex)
                        .ok()
                        .flatten()?
                        .calls;
                    Some(((*rel).clone(), *content, calls))
                })
                .collect();
            for (rel, content, calls) in per_file {
                let mut refs: Vec<CallRef> = Vec::with_capacity(calls.len());
                for call in calls {
                    charged += (call.callee.len() + rel.as_bytes().len()) as u64
                        + (std::mem::size_of::<InRamCall>() + std::mem::size_of::<CallRef>()) as u64;
                    if budget_bytes != 0 && charged > budget_bytes {
                        capped = true;
                        break;
                    }
                    if let Some(key) =
                        crate::index::keys::call_by_callee_hit(&call.callee, &content, call.start_byte, &rel)
                    {
                        entries.push(InRamCall {
                            key,
                            callee: call.callee.clone(),
                            rel: rel.clone(),
                            start_byte: call.start_byte,
                            line: call.start_row + 1,
                            column: call.start_col,
                        });
                    }
                    refs.push(CallRef {
                        line: call.start_row + 1,
                        column: call.start_col,
                        callee: call.callee,
                        start_byte: call.start_byte,
                    });
                }
                by_path.insert(rel, refs);
                if capped {
                    break;
                }
            }
            if capped {
                break;
            }
        }
        entries.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        Self {
            entries,
            by_path,
            capped,
        }
    }

    /// A projection of no files.
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            entries: Vec::new(),
            by_path: ahash::AHashMap::new(),
            capped: false,
        }
    }

    /// Whether the byte budget truncated this projection.
    pub(crate) fn capped(&self) -> bool {
        self.capped
    }

    /// All call sites in `rel` (the `calls_by_path` keyspace).
    pub(crate) fn calls_in_file(&self, rel: &crate::path::RelPath) -> &[CallRef] {
        self.by_path.get(rel).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
#[path = "helpers_calls_scan_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "helpers_calls_group_tests.rs"]
mod group_tests;
