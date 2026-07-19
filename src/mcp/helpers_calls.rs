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
pub(crate) use super::helpers_calls_scan::InRamCallIndex;
use super::helpers_calls_scan::{CallScanPage, budget_call_page, scan_calls};
use super::types::ReferenceHit;
use crate::extract::Call;
use crate::index::IndexDb;
use crate::path::RelPath;

/// How `find_callers` fetches the cross-file resolved uses of a definition. The precise
/// (`resolved: true`) path needs the fjall `refs_by_def` reverse index; under Seam B a
/// `daemon_writer` serve has no open index, so it forwards the lookup to the machine daemon (the
/// sole fjall writer, which holds it). Every other serve resolves locally from its open index — or,
/// read-only without a daemon, the intra-file `.rref` blobs.
pub(super) enum RefsSource<'a> {
    /// Resolve against this serve's own store: the open fjall index, else intra-file `.rref` blobs.
    Local(&'a crate::store::Store),
    /// Forward the lookup to the machine daemon — the `daemon_writer` (read-only serve) path.
    #[cfg(all(feature = "comms", any(unix, windows)))]
    Daemon {
        /// Cached broker client for this session's identity.
        client: std::sync::Arc<tokio::sync::Mutex<crate::comms::client::CommsClient>>,
        /// Canonical workspace root, selecting the daemon's hot workspace.
        root: std::path::PathBuf,
    },
}

impl RefsSource<'_> {
    /// The resolved uses of the definition at `(def_path, def_start)`. On the daemon path a forward
    /// failure degrades to empty — `find_callers` then falls back to the name-based scan, exactly as
    /// a resolution miss does — so a transient daemon hiccup never errors the tool.
    async fn references_to(&self, def_path: &RelPath, def_start: u32) -> Vec<(RelPath, u32)> {
        match self {
            RefsSource::Local(store) => crate::query::resolved_references(store, def_path, def_start),
            #[cfg(all(feature = "comms", any(unix, windows)))]
            RefsSource::Daemon { client, root } => {
                use crate::comms::resolved_proto::{ResolvedRefQuery, ResolvedRefResult};
                let query = ResolvedRefQuery::ReferencesTo {
                    def_path: def_path.clone(),
                    def_start,
                };
                match client.lock().await.resolved_refs(root.clone(), query).await {
                    Ok(ResolvedRefResult::References(uses)) => uses,
                    Ok(_) => Vec::new(),
                    Err(error) => {
                        tracing::debug!(%error, "find_callers: daemon resolved-refs forward failed; name-based fallback");
                        Vec::new()
                    }
                }
            }
        }
    }
}

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
    (call.start_row + 1, call.start_col)
}

/// Body of the `find_references` MCP tool — pulled out so the `#[tool]` wrapper in
/// `tools.rs` stays small. Takes a snapshot of the IndexDb (cheap clone) so the caller
/// can release the store lock before iterating.
pub(super) fn run_find_references(
    idx: Option<&crate::index::IndexDb>,
    params: super::types::FindReferencesParams,
    cache: &super::MapCache,
    notice: Option<super::types::LifecycleNotice>,
    started: std::time::Instant,
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
            notice,
            elapsed_us: super::helpers::elapsed_us(started),
        },
        format,
    )
}

/// Body of the `find_callers` MCP tool.
///
/// ## Semantics: the name scan is the floor, resolution is a refinement
///
/// The name-based `calls_by_callee` scan — the exact scan `find_references` runs — is ALWAYS
/// executed and always defines `total` and `hits`. Scope/import resolution then *refines* that set:
/// it annotates each hit with [`ReferenceHit::resolved`] and contributes `resolved_total`, and it
/// can ADD call sites the name scan structurally cannot see (a renamed local binding —
/// `import { f as g }` then `g()`). It can never remove one.
///
/// This inverts the previous behaviour, which returned the resolved edges *instead of* the scan
/// whenever the definition resolved. That was a silent-wrong-answer generator: resolution has blind
/// spots it cannot detect — a module-object import (`from pkg import mod` then `mod.f()`) binds a
/// module, not `f`, so `intel::xfile` finds no export to bind and skips the importer entirely; ditto
/// unresolvable path aliases. On a real 82 k-file monorepo that reported `total: 2` (both in the
/// definition's own file) for a hook with 172 call sites across 159 files — with no truncation flag,
/// so an agent reasonably concluded almost nothing called it. A precise-looking small number is far
/// more dangerous than an error, and resolution cannot prove the negative it was being trusted for.
///
/// Precision is not lost, only moved: the same-name call sites resolution *disproves* are still
/// reported, but flagged `resolved: false`, so a caller that wants precision filters on the flag
/// instead of being silently handed a subset.
///
/// Holds the store read guard for the call (like `goto_definition`): the resolution layer reads the
/// concurrently-readable `.rref` blobs plus, when open, the Fjall index; the scan reads
/// `store.index_db` or the in-RAM call cache for a read-only multi-session serve.
pub(super) async fn run_find_callers(
    store: &crate::store::Store,
    refs: RefsSource<'_>,
    root: &std::path::Path,
    cache: &super::MapCache,
    params: super::types::FindCallersParams,
    notice: Option<super::types::LifecycleNotice>,
    started: std::time::Instant,
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

    let cursor_bytes = params.cursor.as_ref().map(|c| c.decode_fjall()).transpose()?;
    // The sound floor. Never skipped, never overridden — this is what makes the answer complete.
    let scan = scan_calls(
        store.index_db.as_ref(),
        cache,
        &params.name,
        limit,
        cursor_bytes.as_deref(),
    )?;

    // The refinement. `None` when the definition doesn't resolve (no engine for the language, or no
    // resolution facts): hits then carry no `resolved` annotation at all, rather than a misleading
    // `false` that would imply resolution had ruled them out.
    let resolved = match symbol.as_ref() {
        Some(sym) => resolved_callers(store, &refs, root, cache, &params.path, sym, &params.name, limit).await,
        None => None,
    };
    let resolved_total = resolved.as_ref().map_or(0, |r| r.total);

    let page = merge_resolved(scan, resolved, limit, cursor_bytes.as_deref());
    let total = page.total;
    let total_is_partial = page.total_is_partial;
    let budgeted = budget_call_page(page, params.max_tokens);
    json_result(&FindCallersResponse {
        definition,
        resolved_total,
        total,
        total_is_partial,
        budgeted: budgeted.budgeted,
        hits: budgeted.hits,
        next_cursor: budgeted.next_cursor,
        notice,
        elapsed_us: super::helpers::elapsed_us(started),
    })
}

/// The scope/import-resolved callers of a definition — the refinement layer over the name scan.
///
/// Deliberately NOT a complete caller set: it is everything resolution can *prove*, which is a
/// subset of the truth whose gap resolution cannot measure. See [`run_find_callers`].
struct ResolvedCallers {
    /// `path` → proven call-site start bytes. Probed per name-scan hit to set `resolved`.
    /// A map-of-sets rather than a set of `(RelPath, u32)` so probing borrows the hit's path
    /// instead of cloning it per hit on the scan's inner loop.
    sites: ahash::AHashMap<crate::path::RelPath, ahash::AHashSet<u32>>,
    /// Proven call sites the name scan structurally cannot reach: the callee identifier does not
    /// contain `name`, because the local binding was renamed at the import (`import { f as g }` →
    /// callee `g`). Carried with their `calls_by_callee` key so they merge into the scan's
    /// key-ordered cursor stream rather than perturbing it.
    aliased: Vec<(Vec<u8>, ReferenceHit)>,
    /// Total proven call sites, repo-wide and page-independent.
    total: u32,
}

/// A call site exactly as the index records it.
struct CallSite {
    callee: String,
    /// 1-based.
    line: u32,
    /// 0-based byte column.
    column: u32,
}

/// Every call site in `path`, keyed by start byte, from whichever backend is live. Same
/// dual-backend dispatch as [`for_each_call_in_file`], but it carries line/column so resolved hits
/// get their position from the index — the same source the name scan uses — instead of re-reading
/// and re-scanning the file for a byte offset.
fn call_sites_in_file(
    idx: Option<&IndexDb>,
    cache: &MapCache,
    path: &RelPath,
) -> Result<ahash::AHashMap<u32, CallSite>, McpError> {
    let mut sites: ahash::AHashMap<u32, CallSite> = ahash::AHashMap::new();
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
                sites.insert(
                    call.start_byte,
                    CallSite {
                        callee: call.callee,
                        line: call.start_row + 1,
                        column: call.start_col,
                    },
                );
            }
        }
        None => {
            if let Some(calls) = cache.calls.as_ref() {
                for cref in calls.calls_in_file(path) {
                    sites.insert(
                        cref.start_byte,
                        CallSite {
                            callee: cref.callee.clone(),
                            line: cref.line,
                            column: cref.column,
                        },
                    );
                }
            }
        }
    }
    Ok(sites)
}

/// Build the resolved refinement for the definition `symbol` in `def_path`, or `None` when nothing
/// resolves. Cross-file uses come from the index via [`RefsSource`]: read locally when it is open,
/// else forwarded to the machine daemon on a `daemon_writer` serve.
///
/// Offset alignment (verified empirically, see the unit test): the resolver records `def_start` as
/// the definition *identifier* byte, which is NOT the L1 `Symbol.start_byte` (the definition *node*
/// start — e.g. the `function`/`export` keyword). So the true `def_start`(s) are recovered from the
/// file's resolution blob: intra edges whose `def_start` falls inside the symbol's node span
/// `[start_byte, end_byte)` AND whose identifier text equals `symbol.name`. That both bridges the
/// offset gap and disambiguates same-named definitions living in other scopes.
///
/// Work is bounded by `probe_cap` use-files, mirroring the scan's `scan_cap` convention. Past the
/// cap the refinement simply stops proving things — hits stay unannotated and the floor stays
/// complete. Degrading toward "unproven" is always safe; degrading toward "no callers" is what this
/// function exists to prevent.
// Threads the store, the ref backend, the definition, and the name/cap bounds; a params struct used
// at exactly one call site would obscure more than it saves.
#[allow(clippy::too_many_arguments)]
async fn resolved_callers(
    store: &crate::store::Store,
    ref_source: &RefsSource<'_>,
    root: &std::path::Path,
    cache: &MapCache,
    def_path: &crate::path::RelPath,
    symbol: &crate::extract::Symbol,
    name: &str,
    limit: usize,
) -> Option<ResolvedCallers> {
    let entry = store.lookup(def_path)?;
    let refs = store.read_resolved_by_hex(&entry.hash_hex).ok().flatten()?;
    let def_source = std::fs::read(root.join(def_path.to_path_buf())).ok()?;

    let mut def_starts: Vec<u32> = Vec::new();
    let push_candidate = |byte: u32, def_starts: &mut Vec<u32>| {
        if byte >= symbol.start_byte
            && byte < symbol.end_byte
            && super::helpers_intel::identifier_at(&def_source, byte) == symbol.name.as_str()
            && !def_starts.contains(&byte)
        {
            def_starts.push(byte);
        }
    };
    for edge in &refs.intra {
        push_candidate(edge.def_start, &mut def_starts);
    }
    // Also seed from this file's exports. A definition that is exported and called only from OTHER
    // files (e.g. a Python `def f` in a module that never calls `f` itself) has no intra edge to
    // recover `def_start` from, so intra-only seeding would miss every cross-file caller. The export
    // records the identifier byte the cross-file join keyed on.
    for export in &refs.exports {
        push_candidate(export.name_start, &mut def_starts);
    }
    if def_starts.is_empty() {
        return None;
    }

    // Popular exported symbols have many cross-file uses, so dedup via a hash set rather than a
    // quadratic `Vec::contains` over `(RelPath, u32)` (a full path comparison each probe).
    let mut seen: ahash::AHashSet<(crate::path::RelPath, u32)> = ahash::AHashSet::new();
    let mut uses: Vec<(crate::path::RelPath, u32)> = Vec::new();
    for def_start in def_starts {
        for use_ref in ref_source.references_to(def_path, def_start).await {
            if seen.insert(use_ref.clone()) {
                uses.push(use_ref);
            }
        }
    }
    if uses.is_empty() {
        return None;
    }

    // Resolved edges also cover non-call references — chiefly the `import` binding that introduced
    // the name — so keep only uses that coincide with an actual call site. Those dropped edges still
    // serve `goto_definition` / `find_references`; they just aren't "callers".
    let probe_cap = limit.saturating_mul(8).max(2_000);
    let finder = memchr::memmem::Finder::new(name.as_bytes());
    let mut sites: ahash::AHashMap<crate::path::RelPath, ahash::AHashSet<u32>> = ahash::AHashMap::new();
    let mut aliased: Vec<(Vec<u8>, ReferenceHit)> = Vec::new();
    let mut total: u32 = 0;

    // One index range-scan per use-FILE (not per use), so a definition with many callers in one file
    // costs one scan. `uses` is grouped by path first to guarantee that.
    uses.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut probed = 0usize;
    let mut file_calls: Option<(&crate::path::RelPath, ahash::AHashMap<u32, CallSite>)> = None;
    for (use_path, use_start) in &uses {
        if file_calls.as_ref().is_none_or(|(p, _)| *p != use_path) {
            if probed >= probe_cap {
                break;
            }
            probed += 1;
            // An index hiccup here costs only the annotation, never the floor: the name scan has
            // already run and its hits stand on their own.
            file_calls = Some((
                use_path,
                call_sites_in_file(store.index_db.as_ref(), cache, use_path).ok()?,
            ));
        }
        let Some((_, calls)) = file_calls.as_ref() else {
            continue;
        };
        let Some(site) = calls.get(use_start) else {
            continue; // a resolved reference that isn't a call — e.g. the import binding itself
        };
        total += 1;
        sites.entry(use_path.clone()).or_default().insert(*use_start);
        if finder.find(site.callee.as_bytes()).is_none()
            && let Some(key) = crate::index::keys::call_by_callee(&site.callee, use_path, *use_start)
        {
            // The name scan cannot see this call site (the binding was renamed at the import), so
            // resolution is the ONLY way it ever gets reported. Merge it in rather than drop it.
            aliased.push((
                key,
                ReferenceHit {
                    path: use_path.clone(),
                    line: site.line,
                    column: site.column,
                    callee: site.callee.clone(),
                    resolved: Some(true),
                },
            ));
        }
    }
    if total == 0 {
        return None;
    }
    Some(ResolvedCallers { sites, aliased, total })
}

/// Fold the resolved refinement into the name-scan page: annotate each scan hit with whether
/// resolution proved it, and merge in the aliased call sites the scan cannot see.
///
/// The merge is by `calls_by_callee` key — the same total order the scan iterates — so the emitted
/// page stays a contiguous prefix of one key-ordered stream and the cursor contract is unchanged:
/// `next_cursor` is the last key emitted, and the next call resumes strictly after it. Aliased hits
/// already emitted (key <= cursor) are filtered out, so paging never duplicates them.
fn merge_resolved(
    mut page: CallScanPage,
    resolved: Option<ResolvedCallers>,
    limit: usize,
    cursor_after: Option<&[u8]>,
) -> CallScanPage {
    let Some(resolved) = resolved else {
        return page;
    };

    for (hit, start) in page.hits.iter_mut().zip(page.hit_starts.iter()) {
        let proven = resolved.sites.get(&hit.path).is_some_and(|s| s.contains(start));
        hit.resolved = Some(proven);
    }

    if resolved.aliased.is_empty() {
        return page;
    }
    // Aliased sites are invisible to the name scan, so they are additive to `total` — the count the
    // agent reads as "how many things call this".
    let total = page.total.saturating_add(resolved.aliased.len() as u32);
    let scan_had_more = page.next_cursor.is_some();

    let mut merged: Vec<(Vec<u8>, ReferenceHit, u32)> = Vec::with_capacity(page.hits.len() + resolved.aliased.len());
    for ((key, hit), start) in page
        .hit_keys
        .drain(..)
        .zip(page.hits.drain(..))
        .zip(page.hit_starts.drain(..))
    {
        merged.push((key, hit, start));
    }
    for (key, hit) in resolved.aliased {
        if cursor_after.is_some_and(|cursor| key.as_slice() <= cursor) {
            continue; // already emitted on an earlier page
        }
        // Start byte is only carried to annotate scan hits (done above); aliased hits are already
        // annotated `resolved: true`, so the parallel slot is inert for them.
        merged.push((key, hit, 0));
    }
    merged.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let has_more = merged.len() > limit || scan_had_more;
    merged.truncate(limit);
    let next_cursor = if has_more {
        merged.last().map(|(key, _, _)| Cursor::encode_fjall(key))
    } else {
        None
    };
    let (hit_keys, hits, hit_starts) = merged.into_iter().fold(
        (Vec::new(), Vec::new(), Vec::new()),
        |(mut keys, mut hits, mut starts), (key, hit, start)| {
            keys.push(key);
            hits.push(hit);
            starts.push(start);
            (keys, hits, starts)
        },
    );
    CallScanPage {
        total,
        total_is_partial: page.total_is_partial,
        hits,
        next_cursor,
        hit_keys,
        hit_starts,
    }
}

#[cfg(test)]
mod tests {
    use super::super::helpers_calls_scan::{InRamCallIndex, scan_calls_in_ram};
    use crate::config::ConfigV1;
    use crate::scanner::{ScanSource, scan};
    use crate::store::{Store, VIEW_WORKING};

    /// Decode a `CallToolResult` back into JSON — the exact bytes an MCP client sees.
    fn decode(result: &rmcp::model::CallToolResult) -> serde_json::Value {
        use rmcp::model::ContentBlock;
        let text = result
            .content
            .iter()
            .find_map(|c| match c {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        serde_json::from_str(&text).expect("tool response is JSON")
    }

    /// Scan `root` and return the store plus its map cache.
    fn scan_fixture(root: &std::path::Path) -> (Store, crate::mcp::MapCache) {
        let mut store = Store::open(root, VIEW_WORKING).expect("open");
        scan(
            root,
            &mut store,
            &ConfigV1::with_defaults(),
            ScanSource::WorkingTree,
            crate::scanner::EmbedMode::Inline,
        )
        .expect("scan");
        let cache = crate::mcp::MapCache::build(&store);
        (store, cache)
    }

    /// **P0 regression pin.** `find_callers` MUST report the cross-file callers that scope
    /// resolution cannot see, and MUST NOT present a resolution-limited subset as the complete
    /// answer.
    ///
    /// The fixture is the exact real-world shape that broke: Python's `from pkg import mod`
    /// followed by `mod.target()` binds a *module*, not the function, so the cross-file join
    /// (`src/intel/xfile.rs`) has no named export to bind and skips the importer entirely — every
    /// such caller is invisible to resolution. Meanwhile the definition file's own self-call DOES
    /// resolve intra-file (tree-sitter `locals`, no feature flag), so resolution "succeeds" with a
    /// single hit. A resolution-first `find_callers` therefore returned `total: 1`, no truncation
    /// flag, and an agent concluded almost nothing called `target`.
    ///
    /// The name scan is the sound floor: `find_callers` must agree with `find_references` on the
    /// count for an unambiguous name, and merely *annotate* which hits resolution could prove.
    #[test]
    fn find_callers_reports_cross_file_callers_resolution_cannot_see() {
        use crate::mcp::types::{FindCallersParams, FindReferencesParams};
        use crate::path::RelPath;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir(root.join("pkg")).expect("pkg dir");
        std::fs::write(root.join("pkg/__init__.py"), b"").expect("__init__.py");
        // The self-call is what makes resolution "succeed" — and what made the bug silent.
        std::fs::write(
            root.join("pkg/mod.py"),
            b"def target():\n    return 1\n\n\ndef seed():\n    return target()\n",
        )
        .expect("mod.py");
        // Module-import form: the resolver cannot bind `mod.target` back to `pkg/mod.py::target`.
        std::fs::write(
            root.join("caller_a.py"),
            b"from pkg import mod\n\n\ndef go():\n    return mod.target()\n",
        )
        .expect("caller_a.py");
        std::fs::write(
            root.join("caller_b.py"),
            b"from pkg import mod\n\n\ndef go2():\n    return mod.target() + mod.target()\n",
        )
        .expect("caller_b.py");

        let (store, cache) = scan_fixture(root);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        // Ground truth (matches `git grep -c 'target('` on this fixture): 4 call sites — one in
        // pkg/mod.py (seed's self-call) and three across the two importers.
        let references = decode(
            &super::run_find_references(
                store.index_db.as_ref(),
                FindReferencesParams {
                    name: "target".to_string(),
                    limit: Some(500),
                    max_tokens: None,
                    format: None,
                    cursor: None,
                },
                &cache,
                None,
                std::time::Instant::now(),
            )
            .expect("find_references"),
        );
        assert_eq!(
            references.get("total").and_then(serde_json::Value::as_u64),
            Some(4),
            "ground truth: find_references sees all 4 target() call sites: {references}"
        );

        let callers = decode(
            &runtime
                .block_on(super::run_find_callers(
                    &store,
                    super::RefsSource::Local(&store),
                    root,
                    &cache,
                    FindCallersParams {
                        path: RelPath::from("pkg/mod.py".as_bytes()),
                        name: "target".to_string(),
                        kind: None,
                        limit: Some(500),
                        max_tokens: None,
                        cursor: None,
                    },
                    None,
                    std::time::Instant::now(),
                ))
                .expect("find_callers"),
        );

        assert_eq!(
            callers.get("total").and_then(serde_json::Value::as_u64),
            Some(4),
            "find_callers must NOT shrink to the resolution-visible subset: {callers}"
        );
        let hits = callers.get("hits").and_then(serde_json::Value::as_array).expect("hits");
        let paths: Vec<&str> = hits
            .iter()
            .filter_map(|h| h.get("path").and_then(serde_json::Value::as_str))
            .collect();
        assert!(
            paths.contains(&"caller_a.py") && paths.contains(&"caller_b.py"),
            "the module-import callers resolution cannot see must still be reported: {paths:?}"
        );
        assert_eq!(
            callers.get("total").and_then(serde_json::Value::as_u64),
            references.get("total").and_then(serde_json::Value::as_u64),
            "find_callers and find_references must agree on an unambiguous name"
        );

        // Whatever resolution manages to prove, it is a SUBSET count — never the total. (With a
        // Python engine compiled in — `code-intel-stack` — the self-call resolves and this is where
        // the old code early-returned with `total: 1`. With no engine it proves nothing. Either way
        // the floor above must hold.)
        let resolved_total = callers
            .get("resolved_total")
            .and_then(serde_json::Value::as_u64)
            .expect("resolved_total is always reported");
        assert!(
            resolved_total <= 4,
            "resolved_total is a lower bound on the truth, never above total: {callers}"
        );
    }

    /// **P0 regression pin, JS/TS engine.** The reported production case: a TS hook whose
    /// `find_callers` returned `total: 2` (both in its own file) while `find_references` returned
    /// 172 across 159 files.
    ///
    /// Reproduced exactly: `util.ts` calls `target()` itself, so oxc resolves that intra edge and
    /// resolution "succeeds". `consumer.ts` reaches it through a NAMESPACE import
    /// (`import * as util`), which binds a module object, not `target` — `intel::xfile` finds no
    /// named export to bind and skips the importer, so its two call sites are invisible to
    /// resolution. The old resolution-first code therefore returned `total: 1`, no truncation flag,
    /// and an agent concluded nothing external called the hook.
    #[cfg(feature = "code-intel-js")]
    #[test]
    fn find_callers_reports_namespace_import_callers_the_js_resolver_cannot_bind() {
        use crate::mcp::types::FindCallersParams;
        use crate::path::RelPath;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        // The self-call is what makes resolution "succeed" — and what made the bug silent.
        std::fs::write(
            root.join("util.ts"),
            b"export function target() { return 1; }\ntarget();\n",
        )
        .expect("util.ts");
        // Namespace import: the resolver cannot bind `util.target` back to util.ts's `target`.
        std::fs::write(
            root.join("consumer.ts"),
            b"import * as util from './util';\nutil.target();\nutil.target();\n",
        )
        .expect("consumer.ts");
        let (store, cache) = scan_fixture(root);

        let body = decode(
            &tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(super::run_find_callers(
                    &store,
                    super::RefsSource::Local(&store),
                    root,
                    &cache,
                    FindCallersParams {
                        path: RelPath::from("util.ts".as_bytes()),
                        name: "target".to_string(),
                        kind: None,
                        limit: Some(500),
                        max_tokens: None,
                        cursor: None,
                    },
                    None,
                    std::time::Instant::now(),
                ))
                .expect("find_callers"),
        );

        assert_eq!(
            body.get("total").and_then(serde_json::Value::as_u64),
            Some(3),
            "all 3 call sites — the resolvable self-call AND the 2 namespace-import callers: {body}"
        );
        assert_eq!(
            body.get("resolved_total").and_then(serde_json::Value::as_u64),
            Some(1),
            "resolution can only PROVE the intra-file self-call — it must not pass that off as the total"
        );
        let hits = body.get("hits").and_then(serde_json::Value::as_array).expect("hits");
        let consumer_hits = hits
            .iter()
            .filter(|h| h.get("path").and_then(serde_json::Value::as_str) == Some("consumer.ts"))
            .count();
        assert_eq!(
            consumer_hits, 2,
            "the namespace-import callers resolution cannot see must still be reported: {body}"
        );
        assert!(
            hits.iter().all(|h| h.get("resolved").is_some()),
            "every hit is annotated so the agent can tell proven from unproven: {body}"
        );
    }

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

    /// Precision is preserved, but as an ANNOTATION rather than a silent filter: resolution proves
    /// only the callers that bind to *this* definition (never a same-named function in another
    /// file), yet the same-named site is still REPORTED — flagged `resolved: false` — instead of
    /// being dropped from the answer. Dropping is what made `find_callers` under-report; a caller
    /// that wants precision filters on the flag.
    ///
    /// Also pins the offset-alignment finding: the L1 node `start_byte` differs from the resolver's
    /// `def_start` identifier byte, so the proven set can only be built by recovering the true
    /// `def_start` from the blob. Feature-gated — only oxc (JS/TS) resolves top-level function calls
    /// to their definition today.
    #[cfg(feature = "code-intel-js")]
    #[test]
    fn find_callers_annotates_resolved_edges_without_dropping_same_name_sites() {
        use crate::mcp::types::FindCallersParams;
        use crate::path::RelPath;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join("util.ts"),
            b"export function target() { return 1; }\ntarget();\ntarget();\n",
        )
        .expect("util.ts");
        std::fs::write(root.join("other.ts"), b"function target() { return 3; }\ntarget();\n").expect("other.ts");
        let (store, cache) = scan_fixture(root);

        let def_path = RelPath::from("util.ts".as_bytes());
        let l1 = crate::query::file_outline(&store, &def_path).expect("outline");
        let sym = l1
            .symbols
            .iter()
            .find(|s| s.name == "target" && s.kind == crate::extract::SymbolKind::Function)
            .cloned()
            .expect("util.ts target function symbol");

        let entry = store.lookup(&def_path).expect("indexed");
        let refs = store
            .read_resolved_by_hex(&entry.hash_hex)
            .expect("read blob")
            .expect("resolution facts present");
        assert!(
            !refs.intra.iter().any(|e| e.def_start == sym.start_byte),
            "L1 node start_byte must differ from the resolver's def identifier byte"
        );

        // `run_find_callers` is async (the daemon-forward path awaits a socket); drive it on a
        // throwaway current-thread runtime so `scan` above stays OUTSIDE any runtime (a scan that
        // flushes vectors block_on's its own runtime — nesting would panic). Local resolver: this
        // store has an open index, so the resolution reads it directly with no daemon involved.
        let body = decode(
            &tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(super::run_find_callers(
                    &store,
                    super::RefsSource::Local(&store),
                    root,
                    &cache,
                    FindCallersParams {
                        path: def_path.clone(),
                        name: "target".to_string(),
                        kind: None,
                        limit: Some(100),
                        max_tokens: None,
                        cursor: None,
                    },
                    None,
                    std::time::Instant::now(),
                ))
                .expect("find_callers"),
        );

        assert_eq!(
            body.get("resolved_total").and_then(serde_json::Value::as_u64),
            Some(2),
            "exactly the two util.ts callers are PROVEN to bind to util.ts target: {body}"
        );
        assert_eq!(
            body.get("total").and_then(serde_json::Value::as_u64),
            Some(3),
            "the other.ts same-name call site is still reported, not dropped: {body}"
        );
        let hits = body.get("hits").and_then(serde_json::Value::as_array).expect("hits");
        let proven: Vec<&str> = hits
            .iter()
            .filter(|h| h.get("resolved").and_then(serde_json::Value::as_bool) == Some(true))
            .filter_map(|h| h.get("path").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(
            proven,
            vec!["util.ts", "util.ts"],
            "only the util.ts sites are proven — other.ts is never conflated INTO the proven set"
        );
        let unproven: Vec<&str> = hits
            .iter()
            .filter(|h| h.get("resolved").and_then(serde_json::Value::as_bool) == Some(false))
            .filter_map(|h| h.get("path").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(
            unproven,
            vec!["other.ts"],
            "the same-name site is surfaced as unproven, so a precision-seeking caller can filter it"
        );
    }
}
