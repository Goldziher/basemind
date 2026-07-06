//! Helper bodies for the `find_references` / `find_callers` MCP tools.
//!
//! Extracted out of `helpers.rs` so the parent file stays under the 1000-line per-file
//! cap. Both tools share the same `calls_by_callee` range scan; the only difference is
//! `find_callers` resolves a definition first.

use std::ops::Bound;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::MapCache;
use super::cursor::{Cursor, prefix_upper_bound};
use super::helpers::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX, json_result, kind_to_str, parse_kind};
use super::types::ReferenceHit;
use crate::extract::Call;
use crate::index::IndexDb;
use crate::path::RelPath;

/// Invoke `f(callee, start_byte)` for every call site in `path`, from whichever backend
/// is live (Fjall index when open, in-RAM call index for read-only sessions). Returning
/// `false` from `f` stops iteration early — used to enforce per-file scan caps.
///
/// Shared by `helpers_archmap::RepoGraph::build`, `helpers_archmap::run_tier_symbol`,
/// and `helpers_graph::collect_callees_for_name`. Keeping the dual-backend dispatch here
/// removes the duplicate scan loops those callers previously maintained inline.
pub(super) fn for_each_call_in_file<F: FnMut(&str, u32) -> bool>(
    idx: Option<&IndexDb>,
    cache: &MapCache,
    path: &RelPath,
    mut f: F,
) -> Result<(), McpError> {
    match idx {
        Some(idx) => {
            let prefix = crate::index::keys::calls_by_path_prefix(path);
            let upper: Bound<Vec<u8>> = match prefix_upper_bound(&prefix) {
                Some(b) => Bound::Excluded(b),
                None => Bound::Unbounded,
            };
            for guard in idx.calls_by_path.range::<Vec<u8>, _>((Bound::Included(prefix), upper)) {
                let (_, v) = guard
                    .into_inner()
                    .map_err(|e| McpError::internal_error(format!("index iter: {e}"), None))?;
                let call: Call = match rmp_serde::from_slice(&v) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if !f(&call.callee, call.start_byte) {
                    return Ok(());
                }
            }
        }
        None => {
            if let Some(calls) = cache.calls.as_ref() {
                for cref in calls.calls_in_file(path) {
                    if !f(&cref.callee, cref.start_byte) {
                        return Ok(());
                    }
                }
            }
        }
    }
    Ok(())
}

/// Point-lookup a `Call` value in the index by `(path, start_byte)` and return its
/// `(line, column)` as 1-based / 0-based respectively. Falls back to `(0, 0)` when the
/// row/col fields aren't populated (older L2 blobs predating the field's introduction).
pub(super) fn resolve_call_line_col(
    idx: &crate::index::IndexDb,
    rel: &crate::path::RelPath,
    start_byte: u32,
) -> (u32, u32) {
    let key = crate::index::keys::call_by_path(rel, start_byte);
    let value = match idx.calls_by_path.get(key) {
        Ok(Some(v)) => v,
        _ => return (0, 0),
    };
    let call: crate::extract::Call = match rmp_serde::from_slice(&value) {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    // `start_row` is 0-based; emit 1-based for line numbers per editor convention.
    (call.start_row + 1, call.start_col)
}

/// Body of the `find_references` MCP tool — pulled out so the `#[tool]` wrapper in
/// `tools.rs` stays small. Takes a snapshot of the IndexDb (cheap clone) so the caller
/// can release the store lock before iterating.
pub(super) fn run_find_references(
    idx: Option<&crate::index::IndexDb>,
    params: super::types::FindReferencesParams,
    cache: &super::MapCache,
) -> Result<CallToolResult, McpError> {
    use super::types::FindReferencesResponse;
    let format = super::toon::ResponseFormat::parse(params.format.as_deref());
    let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    let cursor_bytes = params.cursor.as_ref().map(|c| c.decode_fjall()).transpose()?;
    let scan = scan_calls(idx, cache, &params.name, limit, cursor_bytes.as_deref())?;
    let total = scan.total;
    let total_is_partial = scan.total_is_partial;
    let budgeted = budget_call_page(scan, params.max_tokens);
    super::toon::format_result(
        &FindReferencesResponse {
            name: params.name,
            total,
            total_is_partial,
            budgeted: budgeted.budgeted,
            hits: budgeted.hits,
            next_cursor: budgeted.next_cursor,
        },
        format,
    )
}

/// Body of the `find_callers` MCP tool. Resolves the definition's L1 symbol (context echoed in
/// `definition`), then **prefers scope/import-resolved callers**: when the definition resolves it
/// returns only precise, scope-correct call sites (no same-name false positives) and marks the
/// response `resolved: true`. When nothing resolves it falls back to the historical name-based
/// callee scan (same `name`-only, no-scope semantics as `find_references`) — no regression.
///
/// Holds the store read guard for the call (like `goto_definition`): the resolved path reads the
/// concurrently-readable `.rref` blobs plus, when open, the Fjall index; the fallback reads
/// `store.index_db` or the in-RAM call cache for a read-only multi-session serve.
pub(super) fn run_find_callers(
    store: &crate::store::Store,
    root: &std::path::Path,
    cache: &super::MapCache,
    params: super::types::FindCallersParams,
) -> Result<CallToolResult, McpError> {
    use super::types::{DefinitionView, FindCallersResponse};
    let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    let kind_filter = params.kind.as_deref().map(parse_kind).transpose()?;
    let symbol = cache.by_path.get(&params.path).and_then(|l1| {
        l1.symbols
            .iter()
            .find(|s| s.name == params.name && kind_filter.is_none_or(|k| s.kind == k))
            .cloned()
    });
    let definition: Option<DefinitionView> = symbol.as_ref().map(|sym| DefinitionView {
        path: params.path.clone(),
        name: sym.name.clone(),
        kind: kind_to_str(sym.kind),
        start_row: sym.start_row,
        start_col: sym.start_col,
    });

    // Resolved mode: precise, scope-correct callers. Only when the definition resolves to a
    // symbol AND that symbol has resolved uses; otherwise fall through to the name scan.
    if let Some(sym) = symbol.as_ref()
        && let Some(page) = resolved_callers_page(store, root, &params.path, &params.name, sym, limit)
    {
        let total = page.total;
        let total_is_partial = page.total_is_partial;
        let budgeted = budget_call_page(page, params.max_tokens);
        return json_result(&FindCallersResponse {
            definition,
            resolved: true,
            total,
            total_is_partial,
            budgeted: budgeted.budgeted,
            hits: budgeted.hits,
            next_cursor: budgeted.next_cursor,
        });
    }

    // Name-scan fallback: substring on the callee, no scope resolution (`Foo::bar()` and `bar()`
    // both match `name="bar"`). Cursor paging applies here.
    let cursor_bytes = params.cursor.as_ref().map(|c| c.decode_fjall()).transpose()?;
    let scan = scan_calls(
        store.index_db.as_ref(),
        cache,
        &params.name,
        limit,
        cursor_bytes.as_deref(),
    )?;
    let total = scan.total;
    let total_is_partial = scan.total_is_partial;
    let budgeted = budget_call_page(scan, params.max_tokens);
    json_result(&FindCallersResponse {
        definition,
        resolved: false,
        total,
        total_is_partial,
        budgeted: budgeted.budgeted,
        hits: budgeted.hits,
        next_cursor: budgeted.next_cursor,
    })
}

/// Build a scope/import-resolved caller page for the definition `symbol` (named `name`) in
/// `def_path`, or `None` when it has no resolved uses (the caller then falls back to the name
/// scan with no regression). Cross-file callers are included when the Fjall index is open; a
/// read-only multi-session serve sees intra-file callers from the `.rref` blob only.
///
/// Offset alignment (verified empirically, see the unit test): the resolver records `def_start`
/// as the definition *identifier* byte, which is NOT the L1 `Symbol.start_byte` (the definition
/// *node* start — e.g. the `function`/`export` keyword). So the true `def_start`(s) are recovered
/// from the file's resolution blob: intra edges whose `def_start` falls inside the symbol's node
/// span `[start_byte, end_byte)` AND whose identifier text equals `name`. That both bridges the
/// offset gap and disambiguates same-named definitions living in other scopes.
///
/// Resolved caller sets are scope-bounded (small), so the page is returned whole — capped at
/// `limit` with `total_is_partial` when exceeded, and no `next_cursor` (unlike the name scan).
fn resolved_callers_page(
    store: &crate::store::Store,
    root: &std::path::Path,
    def_path: &crate::path::RelPath,
    name: &str,
    symbol: &crate::extract::Symbol,
    limit: usize,
) -> Option<CallScanPage> {
    let entry = store.lookup(def_path)?;
    let refs = store.read_resolved_by_hex(&entry.hash_hex).ok().flatten()?;
    let def_source = std::fs::read(root.join(def_path.to_path_buf())).ok()?;

    // Recover the true resolved def identifier byte(s) inside the symbol's node span.
    let mut def_starts: Vec<u32> = Vec::new();
    for edge in &refs.intra {
        if edge.def_start >= symbol.start_byte
            && edge.def_start < symbol.end_byte
            && super::helpers_intel::identifier_at(&def_source, edge.def_start) == name
            && !def_starts.contains(&edge.def_start)
        {
            def_starts.push(edge.def_start);
        }
    }
    if def_starts.is_empty() {
        return None;
    }

    // Collect resolved uses (intra via blob / index; cross-file only when the index is open).
    let mut uses: Vec<(crate::path::RelPath, u32)> = Vec::new();
    for def_start in def_starts {
        for use_ref in crate::query::resolved_references(store, def_path, def_start) {
            if !uses.contains(&use_ref) {
                uses.push(use_ref);
            }
        }
    }
    if uses.is_empty() {
        return None;
    }
    // Deterministic order: (path, byte offset).
    uses.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let total = uses.len() as u32;
    let total_is_partial = uses.len() > limit;
    // Cache per-file source (read once) to map each use offset to line/col + callee identifier.
    let mut source_cache: ahash::AHashMap<crate::path::RelPath, Vec<u8>> = ahash::AHashMap::new();
    let mut hits: Vec<ReferenceHit> = Vec::with_capacity(uses.len().min(limit));
    for (use_path, use_start) in uses.into_iter().take(limit) {
        let source = source_cache.entry(use_path.clone()).or_insert_with(|| {
            if use_path == *def_path {
                def_source.clone()
            } else {
                std::fs::read(root.join(use_path.to_path_buf())).unwrap_or_default()
            }
        });
        let (line, column) = super::helpers_intel::offset_to_line_col(source, use_start);
        let callee = super::helpers_intel::identifier_at(source, use_start);
        hits.push(ReferenceHit {
            path: use_path,
            line,
            column,
            callee,
        });
    }
    Some(CallScanPage {
        total,
        total_is_partial,
        hits,
        next_cursor: None,
        hit_keys: Vec::new(),
    })
}

pub(super) struct CallScanPage {
    pub total: u32,
    pub total_is_partial: bool,
    pub hits: Vec<ReferenceHit>,
    pub next_cursor: Option<Cursor>,
    /// Parallel to `hits`: the Fjall key for each emitted hit. Retained so a token budget can
    /// re-anchor `next_cursor` to the last KEPT hit, not the last scanned one.
    pub hit_keys: Vec<Vec<u8>>,
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
        // Budget kept every hit on the page — leave the original scan cursor untouched.
        return BudgetedCallPage {
            hits: budget.items,
            next_cursor: page.next_cursor,
            budgeted: false,
        };
    }
    // Re-anchor the cursor to the last kept hit. `budgeted` implies at least one drop and a
    // non-empty page, so `kept >= 1` and the index is in range.
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
    // Build the finder once; full-partition substring scan per the B3/I14 spec.
    let finder = memchr::memmem::Finder::new(name.as_bytes());

    let lower: Bound<Vec<u8>> = match cursor_after {
        Some(k) => Bound::Excluded(k.to_vec()),
        None => Bound::Unbounded,
    };
    let mut hits: Vec<ReferenceHit> = Vec::with_capacity(limit.min(64));
    // Parallel to `hits`: the Fjall key for each emitted hit, so a later token budget can
    // re-anchor the cursor to the last KEPT hit instead of the last scanned one.
    let mut hit_keys: Vec<Vec<u8>> = Vec::with_capacity(limit.min(64));
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
        // Case-sensitive substring filter — skip non-matching callees cheaply.
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
            });
            hit_keys.push(k.to_vec());
        } else {
            // We collected a full page; this extra entry proves more remain on disk.
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
fn scan_calls(
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
    }
}

/// In-RAM `scan_calls_by_name` twin over [`InRamCallIndex`]. Same case-sensitive
/// `memmem` substring filter, same `limit` / `scan_cap` / cursor semantics — the
/// entries carry the exact Fjall key the writer would persist, so cursors and
/// scan order round-trip identically between the two paths.
fn scan_calls_in_ram(index: &InRamCallIndex, name: &str, limit: usize, cursor_after: Option<&[u8]>) -> CallScanPage {
    let finder = memchr::memmem::Finder::new(name.as_bytes());
    // Entries are sorted by key, so resume = first entry strictly past the cursor.
    let start = match cursor_after {
        Some(cursor) => index.entries.partition_point(|e| e.key.as_slice() <= cursor),
        None => 0,
    };
    let mut hits: Vec<ReferenceHit> = Vec::with_capacity(limit.min(64));
    let mut hit_keys: Vec<Vec<u8>> = Vec::with_capacity(limit.min(64));
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
    CallScanPage {
        total,
        total_is_partial,
        hits,
        next_cursor,
        hit_keys,
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

/// A call site within a file: the callee identifier and its start byte offset.
pub(crate) struct CallRef {
    pub callee: String,
    pub start_byte: u32,
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

#[cfg(test)]
mod tests {
    use super::{InRamCallIndex, scan_calls_in_ram};
    use crate::config::ConfigV1;
    use crate::scanner::{ScanSource, scan};
    use crate::store::{Store, VIEW_WORKING};

    /// The in-RAM index (built from blobs, used by read-only sessions) must return
    /// the same references the Fjall path would — this is what keeps `find_references`
    /// working for the 2nd+ concurrent session that can't open the Fjall lock.
    #[test]
    fn in_ram_call_index_resolves_references() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").expect("a.rs");
        std::fs::write(root.join("b.rs"), b"fn beta() { alpha(); alpha(); }\n").expect("b.rs");
        let mut store = Store::open(root, VIEW_WORKING).expect("open");
        scan(
            root,
            &mut store,
            &ConfigV1::with_defaults(),
            ScanSource::WorkingTree,
            crate::scanner::EmbedMode::Inline,
        )
        .expect("scan");

        let index = InRamCallIndex::build(&store);
        let page = scan_calls_in_ram(&index, "alpha", 100, None);
        assert_eq!(page.total, 2, "two alpha() call sites in b.rs");
        assert_eq!(page.hits.len(), 2);
        assert!(page.hits.iter().all(|h| h.callee == "alpha"));
        assert!(
            page.hits.iter().all(|h| h.path.as_str() == Some("b.rs")),
            "both references live in b.rs"
        );
    }

    /// Resolved `find_callers` must prefer scope-resolved edges over the name scan: it returns
    /// only the callers that actually bind to *this* definition (never a same-named function in
    /// another file). Also pins the offset-alignment finding: the L1 node `start_byte` differs
    /// from the resolver's `def_start` identifier byte, so the page can only be built by
    /// recovering the true `def_start` from the blob. Feature-gated — only oxc (JS/TS) resolves
    /// top-level function calls to their definition today.
    #[cfg(feature = "code-intel-js")]
    #[test]
    fn find_callers_prefers_resolved_edges_over_name_scan() {
        use crate::path::RelPath;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        // util.ts: the definition + two callers that resolve to it.
        std::fs::write(
            root.join("util.ts"),
            b"export function target() { return 1; }\ntarget();\ntarget();\n",
        )
        .expect("util.ts");
        // other.ts: a same-named function whose caller must NOT be conflated with util.ts's.
        std::fs::write(root.join("other.ts"), b"function target() { return 3; }\ntarget();\n").expect("other.ts");
        let mut store = Store::open(root, VIEW_WORKING).expect("open");
        scan(
            root,
            &mut store,
            &ConfigV1::with_defaults(),
            ScanSource::WorkingTree,
            crate::scanner::EmbedMode::Inline,
        )
        .expect("scan");

        let def_path = RelPath::from("util.ts".as_bytes());
        let l1 = crate::query::file_outline(&store, &def_path).expect("outline");
        let sym = l1
            .symbols
            .iter()
            .find(|s| s.name == "target" && s.kind == crate::extract::SymbolKind::Function)
            .cloned()
            .expect("util.ts target function symbol");

        // Offset-alignment finding: the L1 node start_byte is NOT the resolved def identifier byte.
        let entry = store.lookup(&def_path).expect("indexed");
        let refs = store
            .read_resolved_by_hex(&entry.hash_hex)
            .expect("read blob")
            .expect("resolution facts present");
        assert!(
            !refs.intra.iter().any(|e| e.def_start == sym.start_byte),
            "L1 node start_byte must differ from the resolver's def identifier byte"
        );

        let page =
            super::resolved_callers_page(&store, root, &def_path, "target", &sym, 100).expect("resolved callers found");
        assert_eq!(
            page.total, 2,
            "exactly the two util.ts callers resolve to util.ts target"
        );
        assert!(page.hits.iter().all(|h| h.callee == "target"));
        assert!(
            page.hits.iter().all(|h| h.path.as_str() == Some("util.ts")),
            "other.ts target() (same name, different scope) must NOT be conflated"
        );
    }
}
