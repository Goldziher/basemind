//! Call-scan primitives for `find_references` / `find_callers`: the `calls_by_callee`
//! range scan, its token-budget re-anchoring, and the in-RAM call index built from L2
//! blobs for read-only sessions that can't open the single-holder Fjall index.
//!
//! Extracted from `helpers_calls.rs` to keep that file within the per-file size budget.

use std::ops::Bound;

use rmcp::ErrorData as McpError;

use super::cursor::Cursor;
use super::helpers_calls::resolve_call_line_col;
use super::types::ReferenceHit;

pub(super) struct CallScanPage {
    pub total: u32,
    pub total_is_partial: bool,
    pub hits: Vec<ReferenceHit>,
    pub next_cursor: Option<Cursor>,
    /// Parallel to `hits`: the Fjall key for each emitted hit. Retained so a token budget can
    /// re-anchor `next_cursor` to the last KEPT hit, not the last scanned one.
    pub hit_keys: Vec<Vec<u8>>,
    /// Parallel to `hits`: each hit's call-site start byte. `find_callers` probes it against the
    /// resolved call-site set to annotate `ReferenceHit::resolved` without re-deriving the offset.
    pub hit_starts: Vec<u32>,
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
/// it with no gap or overlap. `max_tokens = None` is a no-op (original page passes through).
pub(super) fn budget_call_page(page: CallScanPage, max_tokens: Option<u32>) -> BudgetedCallPage {
    if max_tokens.is_none() {
        return BudgetedCallPage {
            hits: page.hits,
            next_cursor: page.next_cursor,
            budgeted: false,
        };
    }
    let budget = super::budget::apply_budget(page.hits, max_tokens);
    if !budget.budgeted {
        return BudgetedCallPage {
            hits: budget.items,
            next_cursor: page.next_cursor,
            budgeted: false,
        };
    }
    let kept = budget.items.len();
    let next_cursor = page.hit_keys.get(kept - 1).map(|k| Cursor::encode_fjall(k));
    BudgetedCallPage {
        hits: budget.items,
        next_cursor,
        budgeted: true,
    }
}

/// Shared inner loop for `find_references` / `find_callers`: full-partition scan of
/// `calls_by_callee` with a `memmem` case-sensitive substring filter on the callee name.
/// Materializes up to `limit` hits and caps at `scan_cap = limit * 8` matching entries
/// to bound work on extremely common names.
///
/// When `cursor_after` is `Some`, the scan resumes from the key immediately following
/// the cursor (exclusive). The cursor returned in [`CallScanPage::next_cursor`] is the
/// last key emitted on this page — pass it back on the next call to advance.
fn scan_calls_by_name(
    idx: &crate::index::IndexDb,
    name: &str,
    limit: usize,
    cursor_after: Option<&[u8]>,
) -> Result<CallScanPage, McpError> {
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
    for guard in idx.calls_by_callee.range::<Vec<u8>, _>((lower, Bound::Unbounded)) {
        let (k, _) = guard
            .into_inner()
            .map_err(|e| McpError::internal_error(format!("index iter: {e}"), None))?;
        let Some((callee, rel, start)) = crate::index::keys::parse_call_by_callee(&k) else {
            continue;
        };
        if finder.find(callee.as_bytes()).is_none() {
            continue;
        }
        total += 1;
        matched += 1;
        if hits.len() < limit {
            let (line, column) = resolve_call_line_col(idx, &rel, start);
            hits.push(ReferenceHit {
                path: rel,
                line,
                column,
                callee,
                resolved: None,
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
    }
    let next_cursor = if has_more {
        hit_keys.last().map(|k| Cursor::encode_fjall(k))
    } else {
        None
    };
    Ok(CallScanPage {
        total,
        total_is_partial,
        hits,
        next_cursor,
        hit_keys,
        hit_starts,
    })
}

/// Route a call scan to the Fjall index when it's open, or to the in-RAM index
/// built from the L2 blobs when it isn't.
///
/// `index_db == None` happens on a read-only `serve` session that lost the
/// single-holder Fjall lock to another process (fjall is single-process; see
/// `tests/multisession_smoke.rs`). Such a session still has the concurrently
/// readable blobs, so `find_references` / `find_callers` answer from
/// [`InRamCallIndex`] instead of failing — letting many sessions share one repo.
pub(super) fn scan_calls(
    idx: Option<&crate::index::IndexDb>,
    cache: &super::MapCache,
    name: &str,
    limit: usize,
    cursor_after: Option<&[u8]>,
) -> Result<CallScanPage, McpError> {
    match idx {
        Some(idx) => scan_calls_by_name(idx, name, limit, cursor_after),
        None => Ok(match cache.calls.as_ref() {
            Some(calls) => scan_calls_in_ram(calls, name, limit, cursor_after),
            None => empty_call_page(),
        }),
    }
}

fn empty_call_page() -> CallScanPage {
    CallScanPage {
        total: 0,
        total_is_partial: false,
        hits: Vec::new(),
        next_cursor: None,
        hit_keys: Vec::new(),
        hit_starts: Vec::new(),
    }
}

/// In-RAM `scan_calls_by_name` twin over [`InRamCallIndex`]. Same case-sensitive
/// `memmem` substring filter, same `limit` / `scan_cap` / cursor semantics — the
/// entries carry the exact Fjall key the writer would persist, so cursors and
/// scan order round-trip identically between the two paths.
pub(super) fn scan_calls_in_ram(
    index: &InRamCallIndex,
    name: &str,
    limit: usize,
    cursor_after: Option<&[u8]>,
) -> CallScanPage {
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
    for entry in &index.entries[start..] {
        if finder.find(entry.callee.as_bytes()).is_none() {
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
    let next_cursor = if has_more {
        hit_keys.last().map(|k| Cursor::encode_fjall(k))
    } else {
        None
    };
    CallScanPage {
        total,
        total_is_partial,
        hits,
        next_cursor,
        hit_keys,
        hit_starts,
    }
}

/// In-RAM mirror of the Fjall `calls_by_callee` + `calls_by_path` keyspaces, built
/// from the L2 call blobs for read-only `serve` sessions that can't open the
/// single-holder Fjall index. Lets unlimited concurrent sessions answer
/// `find_references` / `find_callers` / `call_graph` from the shared, immutable,
/// concurrently-readable blobs.
pub(crate) struct InRamCallIndex {
    /// Sorted ascending by `key` to match Fjall's `range` iteration order
    /// (drives `find_references` / `find_callers`).
    entries: Vec<InRamCall>,
    /// path → its call sites (the `calls_by_path` keyspace), for the call-graph
    /// "callees" direction.
    by_path: ahash::AHashMap<crate::path::RelPath, Vec<CallRef>>,
}

struct InRamCall {
    /// `keys::call_by_callee(callee, rel, start_byte)` — the exact key the writer
    /// persists, reused so cursors round-trip identically across the two paths.
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
pub(crate) struct CallRef {
    pub callee: String,
    pub start_byte: u32,
    /// 1-based line (`start_row + 1`), matching [`resolve_call_line_col`].
    pub line: u32,
    /// 0-based byte column.
    pub column: u32,
}

impl InRamCallIndex {
    /// Build the index by decoding the L2 calls from every file's combined blob.
    /// File reads/decodes run in parallel (pure read, like `MapCache::build`); the
    /// two views are assembled serially afterward.
    pub(crate) fn build(store: &crate::store::Store) -> Self {
        use rayon::prelude::*;
        let per_file: Vec<(crate::path::RelPath, Vec<crate::extract::Call>)> = store
            .index
            .files
            .par_iter()
            .filter_map(|(rel, entry)| {
                let calls = store.read_l2_by_hex(&entry.hash_hex).ok().flatten()?.calls;
                Some((rel.clone(), calls))
            })
            .collect();
        let mut entries: Vec<InRamCall> = Vec::new();
        let mut by_path: ahash::AHashMap<crate::path::RelPath, Vec<CallRef>> =
            ahash::AHashMap::with_capacity(per_file.len());
        for (rel, calls) in per_file {
            let mut refs: Vec<CallRef> = Vec::with_capacity(calls.len());
            for call in calls {
                if let Some(key) = crate::index::keys::call_by_callee(&call.callee, &rel, call.start_byte) {
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
        }
        entries.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        Self { entries, by_path }
    }

    /// Call sites whose callee is exactly `name`, as `(path, start_byte)`. Mirrors a
    /// `calls_by_callee` exact-name scan for the call-graph "callers" direction.
    pub(crate) fn callers_of<'a>(
        &'a self,
        name: &'a str,
    ) -> impl Iterator<Item = (&'a crate::path::RelPath, u32)> + 'a {
        self.entries
            .iter()
            .filter(move |c| c.callee == name)
            .map(|c| (&c.rel, c.start_byte))
    }

    /// All call sites in `rel`, for the call-graph "callees" direction (the
    /// `calls_by_path` keyspace).
    pub(crate) fn calls_in_file(&self, rel: &crate::path::RelPath) -> &[CallRef] {
        self.by_path.get(rel).map_or(&[], Vec::as_slice)
    }
}
