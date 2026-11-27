//! Bodies of `code_refs` relation `callers`: the call sites of a name, and the callers of one
//! definition. Both share the same `calls_by_callee` range scan; the second resolves the
//! definition first.

use super::calls_scan::{
    CallRef, CallScanPage, calls_in_files, content_keys, mark_shared_lines, names_call, scan_calls,
};
use super::matching::Matching;
use super::resolution::{UseSources, classify_references, confirm_elsewhere, finish_page};
use super::routes::{Endpoint, handler_links, scan_route_calls};
use crate::engine::MapCache;
use crate::engine::cursor::{Cursor, stale_cursor};
use crate::engine::error::EngineError;
use crate::engine::files::{IndexedFiles, Unreadable, not_indexed_in, outside_repository};
use crate::engine::freshness;
use crate::engine::ops::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX};
use crate::engine::types::code::{kind_to_str, parse_kind};
use crate::engine::types::refs::{
    DefinitionView, FindCallersParams, FindCallersResponse, FindReferencesParams, FindReferencesResponse, ReferenceHit,
    Resolution,
};
use crate::engine::types::{LifecycleNotice, SCAN_CAP};
use crate::index::view::IndexView;

/// The index `store` reads, for a `code_refs` relation that scans it (call sites, callers,
/// implementations); otherwise the refusal naming the fix. A session holds no index when another
/// process holds it and no daemon serves this one (a CLI command or a second `serve` beside a
/// writer), or when the index must be rebuilt. The first is marked busy, so the CLI exits `3`. A
/// daemon-hosted session finds none while the daemon reopens a poisoned handle or after it evicted
/// the workspace; that too is busy, and a rescan opens the workspace again.
pub(crate) fn index_for_refs(store: &crate::store::Store) -> Result<std::sync::Arc<dyn IndexView>, EngineError> {
    if let Some(idx) = store.read_index_view() {
        return Ok(idx);
    }
    if store.index_source.is_some() {
        return Err(EngineError::busy(
            "`code_refs`: the daemon released this workspace's index (it is reopening it, or evicted the \
             workspace): retry, or run `index_rescan` to open it again",
        ));
    }
    if store.rescan_owed() {
        return Err(EngineError::index_unavailable(
            "`code_refs`: the index of this workspace must be rebuilt: run `basemind scan` (MCP: `index_rescan`)",
        ));
    }
    #[cfg(all(feature = "comms", any(unix, windows)))]
    let fix = "start the daemon (`basemind comms start`), which then serves the index to every session, or stop \
               the process that holds the lock, then retry";
    #[cfg(not(all(feature = "comms", any(unix, windows))))]
    let fix = "stop the process that holds the lock, then retry";
    Err(EngineError::busy(format!(
        "`code_refs`: another basemind process holds this workspace's index and no daemon serves it: {fix}"
    )))
}

/// Point-lookup a `Call` value in the index by `(content, start_byte)` and return its
/// `(line, column)` as 1-based / 0-based respectively. The content key is the one the hit key
/// carries, so no second live-key snapshot is taken mid-query. Falls back to `(0, 0)` when the
/// row/col fields aren't populated (older L2 blobs predating the field's introduction).
pub(crate) fn resolve_call_line_col(
    idx: &dyn crate::index::view::IndexView,
    content: &crate::index::keys::ContentKey,
    start_byte: u32,
) -> (u32, u32) {
    let value = match idx.call_at(content, start_byte) {
        Ok(Some(v)) => v,
        _ => return (0, 0),
    };
    let call: crate::extract::Call = match rmp_serde::from_slice(&value) {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    (call.start_row + 1, call.start_col)
}

/// Call sites of a name (`code_refs` without `path`): the `calls_by_callee` scan over `idx`, each
/// hit graded by how it resolves ([`classify_references`]) against `cache`'s outlines and the
/// indexed bytes of `files`. An exact `METHOD /path` name lists the HTTP calls linked to that route
/// by path instead ([`scan_route_calls`]), each graded `path_match`.
pub(crate) fn run_find_references(
    idx: &dyn IndexView,
    files: IndexedFiles<'_>,
    cache: &MapCache,
    params: FindReferencesParams,
    notice: impl FnOnce() -> Option<LifecycleNotice>,
    flag_stale: impl FnOnce(&mut [ReferenceHit]),
) -> Result<FindReferencesResponse, EngineError> {
    let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    let (stamped, cursor_bytes) = unstamp(params.cursor.as_ref())?.unzip();
    let matching = Matching::from_substring(params.substring);
    let route = Some(matching)
        .filter(|matching| *matching == Matching::Exact)
        .and_then(|_| Endpoint::parse(&params.name));
    let scope = params.scope.as_ref();
    let (mut scan, generation) = match &route {
        Some(target) => scan_route_calls(idx, cache, target, limit, cursor_bytes.as_deref(), scope)?,
        None => scan_calls(idx, &params.name, matching, limit, cursor_bytes.as_deref(), scope)?,
    };
    refuse_stale(stamped, generation)?;
    if route.is_none() {
        classify_references(
            idx,
            &mut UseSources::new(files, cache),
            &mut scan.hits,
            &scan.hit_starts,
        );
    }
    mark_shared_lines(idx, &mut scan.hits, &scan.hit_keys);
    let total = scan.total;
    let total_is_partial = scan.total_is_partial;
    let name = route.as_ref().map_or_else(|| params.name.clone(), Endpoint::name);
    let mut finished = finish_page(scan, params.resolution, &name, params.max_tokens);
    flag_stale(&mut finished.page.hits);
    Ok(FindReferencesResponse {
        resolution_default: finished.default,
        resolution_counts: finished.counts,
        total,
        truncated: total_is_partial,
        truncated_reason: total_is_partial.then_some(SCAN_CAP),
        budgeted: finished.page.budgeted,
        hits: finished.page.hits,
        scope_files: None,
        next_cursor: stamp(finished.page.next_cursor, generation),
        notice: notice(),
    })
}

/// Callers of one definition (`code_refs` with `path`).
///
/// ## Semantics: the name scan is the floor, resolution is a refinement
///
/// The name-based `calls_by_callee` scan — the exact scan `find_references` runs — is ALWAYS
/// executed and always defines `total` and `hits`. Scope/import resolution then *refines* that set:
/// it grades each hit's [`ReferenceHit::resolution`] and contributes `resolved_total`, and it
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
/// reported, but graded `elsewhere`, so a caller that wants precision filters on the grade
/// instead of being silently handed a subset.
///
/// Holds the store read guard for the call (like `goto_definition`): the resolution layer reads the
/// concurrently-readable `.rref` blobs plus the Fjall index `idx`, which the scan reads too.
///
/// A `Route` definition has no call sites by name: its callers are the HTTP calls linked to it by
/// path ([`scan_route_calls`]), graded `path_match`, and resolution does not run. A route name is
/// pinned in any parameter syntax (`GET /users/{id}` picks the route `GET /users/:param`).
///
/// The callers of an HTTP handler also list the routes naming it and the HTTP calls those routes
/// answer ([`handler_links`]), graded `path_match` and merged into the pages by key.
pub(crate) fn run_find_callers(
    store: &crate::store::Store,
    idx: &dyn IndexView,
    files: IndexedFiles<'_>,
    cache: &MapCache,
    params: FindCallersParams,
    notice: impl FnOnce() -> Option<LifecycleNotice>,
) -> Result<FindCallersResponse, EngineError> {
    let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    let kind_filter = params.kind.as_deref().map(parse_kind).transpose()?;
    // A pinned call that cannot be pinned is an error: answering with the unpinned name scan would
    // read as "these are the callers of the definition in that file".
    let not_indexed = || not_indexed_in(store, files.root, "code_refs", &params.path);
    // One snapshot for the stamp, the outline and the resolution facts: all three follow the
    // store's entry, which this call holds a read guard on.
    let (entry, l1) = freshness::indexed_outline(store, cache, &params.path).ok_or_else(not_indexed)?;
    let file = files.file(store, &params.path).map_err(|refusal| match refusal {
        Unreadable::NotIndexed => not_indexed(),
        Unreadable::Outside => outside_repository("code_refs", &params.path),
    })?;
    // The outline's spans and the resolution facts below are offsets into the indexed bytes.
    let source = freshness::load_fresh("code_refs", &params.path, &file, (&entry).into(), files.max_file_bytes)?;
    // A route symbol is named in normalized form, which a route name in any parameter syntax parses to.
    let pinned_name = Endpoint::parse(&params.name).map_or_else(|| params.name.clone(), |endpoint| endpoint.name());
    let symbol = crate::engine::pick::pick_symbol(
        "code_refs",
        &params.path,
        None,
        &l1.symbols,
        Some(&source),
        &pinned_name,
        kind_filter,
        params.line,
    )?
    .symbol;
    let definition = DefinitionView {
        path: params.path.clone(),
        name: symbol.name.clone(),
        kind: kind_to_str(symbol.kind),
        line: symbol.start_row + 1,
        column: symbol.start_col + 1,
    };

    let (stamped, cursor_bytes) = unstamp(params.cursor.as_ref())?.unzip();
    let matching = Matching::from_substring(params.substring);
    let route = Some(symbol)
        .filter(|symbol| symbol.kind == crate::extract::SymbolKind::Route)
        .and_then(|symbol| Endpoint::parse(&symbol.name));
    let scope = params.scope.as_ref();
    let (scan, generation) = match &route {
        Some(target) => scan_route_calls(idx, cache, target, limit, cursor_bytes.as_deref(), scope)?,
        None => scan_calls(idx, &params.name, matching, limit, cursor_bytes.as_deref(), scope)?,
    };
    refuse_stale(stamped, generation)?;
    let links = match route {
        Some(_) => None,
        None => {
            let finder = memchr::memmem::Finder::new(params.name.as_bytes());
            let listed = |callee: &str| names_call(&finder, callee, &params.name, matching);
            handler_links(idx, cache, &params.path, &symbol.name, scope, listed)?
        }
    };

    // Resolution facts are a per-file blob the scan writes only for a file that produced some. No
    // blob is not `resolved_total: 0`: the language has no resolution engine, the resolve pass did
    // not finish, or this file bound nothing; `resolution` says which (`intel::resolution_gap`).
    let resolution_ran = route.is_none() && store.read_resolved_by_hex(&entry.hash_hex).ok().flatten().is_some();
    let resolved = if resolution_ran {
        let pinned = PinnedDefinition {
            path: &params.path,
            source: &source,
            symbol,
        };
        resolved_callers(store, idx, pinned, &params.name, limit, scope)
    } else {
        None
    };
    let resolved_total = resolved.as_ref().map(|r| r.total);
    let resolution_note = if resolved.is_some() || route.is_some() {
        None
    } else if resolution_ran {
        Some(super::intel::NO_DEFINITION_FACTS.to_string())
    } else {
        super::intel::resolution_gap(
            cache.language_of(&params.path),
            super::intel::resolve_pass_complete(store),
        )
    };

    let mut page = merge_resolved(scan, resolved, &params.path, limit, cursor_bytes.as_deref());
    if let Some(links) = links {
        page = merge_keyed(page, links.hits, limit, cursor_bytes.as_deref());
        page.total_is_partial |= links.partial;
    }
    confirm_elsewhere(
        idx,
        &mut UseSources::new(files, cache),
        &mut page.hits,
        &page.hit_starts,
    );
    mark_shared_lines(idx, &mut page.hits, &page.hit_keys);
    let total = page.total;
    let total_is_partial = page.total_is_partial;
    let name = route.as_ref().map_or_else(|| params.name.clone(), Endpoint::name);
    let finished = finish_page(page, params.resolution, &name, params.max_tokens);
    Ok(FindCallersResponse {
        definition,
        resolution_note,
        resolution_default: finished.default,
        resolution_counts: finished.counts,
        resolved_total,
        total,
        truncated: total_is_partial,
        truncated_reason: total_is_partial.then_some(SCAN_CAP),
        budgeted: finished.page.budgeted,
        hits: finished.page.hits,
        scope_files: None,
        next_cursor: stamp(finished.page.next_cursor, generation),
        notice: notice(),
    })
}

/// Bytes of the generation a `references` / `callers` cursor starts with.
const GENERATION_LEN: usize = 8;

/// The generation a [`stamp`]ed cursor was cut in, and the scan key after which it resumes.
type Stamped = ([u8; GENERATION_LEN], Vec<u8>);

/// The cursor of a name scan, prefixed with the generation of the key order it was cut in
/// ([`scan_calls`]). Hits are in content-key order, and an edit moves the edited file's
/// hits, so a cursor continues only the view it was cut from.
fn stamp(cursor: Option<Cursor>, generation: u64) -> Option<Cursor> {
    let key = cursor?.decode_fjall().ok()?;
    let mut bytes = Vec::with_capacity(GENERATION_LEN + key.len());
    bytes.extend_from_slice(&generation.to_be_bytes());
    bytes.extend_from_slice(&key);
    Some(Cursor::encode_fjall(&bytes))
}

/// The generation a [`stamp`]ed cursor was cut in and the scan position it names; the stale-cursor
/// error for a cursor too short to carry a generation.
fn unstamp(cursor: Option<&Cursor>) -> Result<Option<Stamped>, EngineError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let bytes = cursor.decode_fjall()?;
    match bytes.split_first_chunk::<GENERATION_LEN>() {
        Some((stamped, key)) => Ok(Some((*stamped, key.to_vec()))),
        None => Err(stale_cursor("code_refs")),
    }
}

/// The stale-cursor error when the page continued a cursor cut in another generation than the one
/// its scan ran in: the view changed since, so the page may skip or repeat hits.
fn refuse_stale(stamped: Option<[u8; GENERATION_LEN]>, generation: u64) -> Result<(), EngineError> {
    match stamped {
        Some(stamped) if stamped != generation.to_be_bytes() => Err(stale_cursor("code_refs")),
        _ => Ok(()),
    }
}

/// The scope/import-resolved callers of a definition — the refinement layer over the name scan.
///
/// Deliberately NOT a complete caller set: it is everything resolution can *prove*, which is a
/// subset of the truth whose gap resolution cannot measure. See [`run_find_callers`].
#[derive(Default)]
pub struct ResolvedCallers {
    /// `path` → proven call-site start bytes. Probed per name-scan hit to set its `resolution`.
    /// A map-of-sets rather than a set of `(RelPath, u32)` so probing borrows the hit's path
    /// instead of cloning it per hit on the scan's inner loop.
    pub(crate) sites: ahash::AHashMap<crate::path::RelPath, ahash::AHashSet<u32>>,
    /// Files holding resolved uses that were past the probe cap: nothing is known about their call
    /// sites, so hits there stay `name_only`.
    pub(crate) unprobed: ahash::AHashSet<crate::path::RelPath>,
    /// Proven call sites the name scan structurally cannot reach: the callee identifier does not
    /// contain `name`, because the local binding was renamed at the import (`import { f as g }` →
    /// callee `g`). Carried with their `calls_by_callee` key so they merge into the scan's
    /// key-ordered cursor stream rather than perturbing it.
    aliased: Vec<(Vec<u8>, ReferenceHit)>,
    /// Total proven call sites in the scope, page-independent.
    total: u32,
}

/// The definition callers are pinned to: its file, the file's indexed bytes and its outline symbol.
struct PinnedDefinition<'a> {
    path: &'a crate::path::RelPath,
    source: &'a [u8],
    symbol: &'a crate::extract::Symbol,
}

/// A call site exactly as the index records it.
struct CallSite {
    callee: String,
    /// 1-based.
    line: u32,
    /// 0-based byte column.
    column: u32,
}

/// A call site as `CallRef` carries it, re-keyed by start byte for the resolved-use probe.
fn index_by_start(calls: Vec<CallRef>) -> ahash::AHashMap<u32, CallSite> {
    calls
        .into_iter()
        .map(|c| {
            (
                c.start_byte,
                CallSite {
                    callee: c.callee,
                    line: c.line,
                    column: c.column,
                },
            )
        })
        .collect()
}

/// How many use-files `resolved_callers` fetches per round trip. Bounds the decoded call sites
/// live at once, and the size of each forwarded `CallsInFiles` request.
const CALL_SITE_BATCH: usize = 128;

/// Build the resolved refinement for the `pinned` definition `symbol` in `def_path`, whose indexed
/// bytes are `def_source`. `None` when resolution has nothing to say about that definition (its facts do
/// not name it, or the call sites could not be fetched): every hit then stays `name_only`. A
/// definition resolution knows and found no use of yields an empty refinement, which grades every
/// hit `elsewhere` until [`confirm_elsewhere`] finds it binds to nothing. Cross-file uses come from the index when it is open, else the intra-file `.rref`
/// blobs.
///
/// Offset alignment (verified empirically, see the unit test): the resolver records `def_start` as
/// the definition *identifier* byte, which is NOT the L1 `Symbol.start_byte` (the definition *node*
/// start — e.g. the `function`/`export` keyword). So the true `def_start`(s) are recovered from the
/// file's resolution blob: intra edges whose `def_start` falls inside the symbol's node span
/// `[start_byte, end_byte)` AND whose identifier text equals `symbol.name`. That both bridges the
/// offset gap and disambiguates same-named definitions living in other scopes.
///
/// Only uses in the files of a `changed` `scope` are probed: the name scan lists no call site
/// elsewhere, so neither the aliased sites nor `resolved_total` count one, and the probe cap is
/// spent on the scope's files alone.
///
/// Work is bounded by `probe_cap` use-files, mirroring the scan's `scan_cap` convention. Past the
/// cap the refinement simply stops proving things — hits stay `name_only` and the floor stays
/// complete. Degrading toward "unchecked" is always safe; degrading toward "no callers" is what this
/// function exists to prevent.
fn resolved_callers(
    store: &crate::store::Store,
    idx: &dyn IndexView,
    pinned: PinnedDefinition<'_>,
    name: &str,
    limit: usize,
    scope: Option<&crate::engine::scope::ChangeScope>,
) -> Option<ResolvedCallers> {
    let PinnedDefinition {
        path: def_path,
        source: def_source,
        symbol,
    } = pinned;
    let entry = store.lookup(def_path)?;
    let refs = store.read_resolved_by_hex(&entry.hash_hex).ok().flatten()?;

    let mut def_starts: Vec<u32> = Vec::new();
    let push_candidate = |byte: u32, def_starts: &mut Vec<u32>| {
        if byte >= symbol.start_byte
            && byte < symbol.end_byte
            && super::intel::identifier_at(def_source, byte) == symbol.name.as_str()
            && !def_starts.contains(&byte)
        {
            def_starts.push(byte);
        }
    };
    for edge in &refs.intra {
        push_candidate(edge.def_start, &mut def_starts);
    }
    for export in &refs.exports {
        push_candidate(export.name_start, &mut def_starts);
    }
    if def_starts.is_empty() {
        return None;
    }

    let mut seen: ahash::AHashSet<(crate::path::RelPath, u32)> = ahash::AHashSet::new();
    let mut uses: Vec<(crate::path::RelPath, u32)> = Vec::new();
    for def_start in def_starts {
        for use_ref in crate::query::resolved_references(store, def_path, def_start) {
            if crate::engine::changed::admits(scope, &use_ref.0) && seen.insert(use_ref.clone()) {
                uses.push(use_ref);
            }
        }
    }
    if uses.is_empty() {
        return Some(ResolvedCallers::default());
    }

    let probe_cap = limit.saturating_mul(8).max(2_000);
    let finder = memchr::memmem::Finder::new(name.as_bytes());
    let mut sites: ahash::AHashMap<crate::path::RelPath, ahash::AHashSet<u32>> = ahash::AHashMap::new();
    let mut aliased: Vec<(Vec<u8>, ReferenceHit)> = Vec::new();
    let mut total: u32 = 0;

    uses.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    // Uses are sorted by path, so each file's uses are one contiguous run. Fetch the files' call
    // sites a batch at a time (one forwarded round trip per batch on a daemon-backed session).
    let mut runs: Vec<(&crate::path::RelPath, &[(crate::path::RelPath, u32)])> = Vec::new();
    for group in uses.chunk_by(|a, b| a.0 == b.0) {
        runs.push((&group[0].0, group));
    }
    let unprobed = runs
        .get(probe_cap..)
        .unwrap_or_default()
        .iter()
        .map(|(path, _)| (*path).clone())
        .collect();
    runs.truncate(probe_cap);
    let paths: Vec<crate::path::RelPath> = runs.iter().map(|(path, _)| (*path).clone()).collect();
    // One snapshot for every file, so the keys agree with each other and with the calls read. ~keep
    let keys = content_keys(idx, &paths);
    for (batch, batch_keys) in runs.chunks(CALL_SITE_BATCH).zip(keys.chunks(CALL_SITE_BATCH)) {
        let files: Vec<(crate::path::RelPath, Option<crate::index::keys::ContentKey>)> = batch
            .iter()
            .map(|(path, _)| (*path).clone())
            .zip(batch_keys.iter().copied())
            .collect();
        let per_file = calls_in_files(idx, &files).ok()?;
        // A file read under no key has no calls, so every proven site below has its key. ~keep
        for ((use_path, run), (content, calls)) in
            batch.iter().zip(per_file).filter_map(|(run, read)| Some((run, read?)))
        {
            let calls = index_by_start(calls);
            for (_, use_start) in *run {
                let Some(site) = calls.get(use_start) else {
                    continue;
                };
                total += 1;
                sites.entry((*use_path).clone()).or_default().insert(*use_start);
                if finder.find(site.callee.as_bytes()).is_some() {
                    continue;
                }
                if let Some(key) = crate::index::keys::call_by_callee_hit(&site.callee, &content, *use_start, use_path)
                {
                    aliased.push((
                        key,
                        ReferenceHit {
                            path: (*use_path).clone(),
                            line: site.line,
                            column: site.column,
                            callee: site.callee.clone(),
                            resolution: proven(use_path, def_path),
                            elide_resolution: false,
                            shared_line: false,
                            stale: false,
                            ambiguous: false,
                        },
                    ));
                }
            }
        }
    }
    Some(ResolvedCallers {
        sites,
        unprobed,
        aliased,
        total,
    })
}

/// The resolution of a call site proven to bind to a definition in `def_path`.
fn proven(use_path: &crate::path::RelPath, def_path: &crate::path::RelPath) -> Resolution {
    if use_path == def_path {
        Resolution::SameFile
    } else {
        Resolution::Imported
    }
}

/// Fold the resolved refinement into the name-scan page: grade each scan hit `same_file` or
/// `imported` when resolution proved it binds to the definition in `def_path`, `elsewhere` when it
/// did not, and `name_only` when its file lay past the probe cap; then merge in the aliased call
/// sites the scan cannot see ([`merge_keyed`]).
pub(crate) fn merge_resolved(
    mut page: CallScanPage,
    resolved: Option<ResolvedCallers>,
    def_path: &crate::path::RelPath,
    limit: usize,
    cursor_after: Option<&[u8]>,
) -> CallScanPage {
    let Some(resolved) = resolved else {
        return page;
    };

    for (hit, start) in page.hits.iter_mut().zip(page.hit_starts.iter()) {
        if resolved.unprobed.contains(&hit.path) {
            continue;
        }
        hit.resolution = if resolved.sites.get(&hit.path).is_some_and(|s| s.contains(start)) {
            proven(&hit.path, def_path)
        } else {
            Resolution::Elsewhere
        };
    }

    merge_keyed(page, resolved.aliased, limit, cursor_after)
}

/// Merge `extra` call sites, each with its `calls_by_callee` key, into a scan page. `extra` holds
/// every such site across all pages, so each counts toward `total` once; the page lists those
/// that sort after `cursor_after` and within the scan's reach.
///
/// The merge is by key, the total order the scan iterates, so the emitted page stays a contiguous
/// prefix of one key-ordered stream: `next_cursor` is the last key emitted, and the next call
/// resumes strictly after it. Sites already emitted (key <= cursor) are filtered out, so paging
/// never duplicates them.
pub(crate) fn merge_keyed(
    mut page: CallScanPage,
    extra: Vec<(Vec<u8>, ReferenceHit)>,
    limit: usize,
    cursor_after: Option<&[u8]>,
) -> CallScanPage {
    if extra.is_empty() {
        return page;
    }
    let total = page.total.saturating_add(extra.len() as u32);
    // A scan that stopped at its examined-keys cap has looked at nothing past `stop`: an aliased
    // hit beyond it belongs to a later page, and the cursor must stay at `stop` for that page to
    // continue the scan from there.
    let stop = page.stopped_after.take();
    let scan_had_more = page.next_cursor.is_some() && stop.is_none();

    let mut merged: Vec<(Vec<u8>, ReferenceHit, u32)> = Vec::with_capacity(page.hits.len() + extra.len());
    for ((key, hit), start) in page
        .hit_keys
        .drain(..)
        .zip(page.hits.drain(..))
        .zip(page.hit_starts.drain(..))
    {
        merged.push((key, hit, start));
    }
    for (key, hit) in extra {
        if cursor_after.is_some_and(|cursor| key.as_slice() <= cursor)
            || stop.as_deref().is_some_and(|stop| key.as_slice() > stop)
        {
            continue;
        }
        merged.push((key, hit, 0));
    }
    merged.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let has_more = merged.len() > limit || scan_had_more;
    merged.truncate(limit);
    let stopped_after = if has_more { None } else { stop };
    let next_cursor = if has_more {
        merged.last().map(|(key, _, _)| Cursor::encode_fjall(key))
    } else {
        stopped_after.as_deref().map(Cursor::encode_fjall)
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
        stopped_after,
        hit_keys,
        hit_starts,
    }
}

#[cfg(test)]
#[path = "calls_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "calls_resolved_tests.rs"]
mod resolved_tests;
