//! Helper bodies for the `find_references` / `find_callers` MCP tools.
//!
//! Extracted out of `helpers.rs` so the parent file stays under the 1000-line per-file
//! cap. Both tools share the same `calls_by_callee` range scan; the only difference is
//! `find_callers` resolves a definition first.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::MapCache;
use super::cursor::Cursor;
use super::helpers::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX, json_result, kind_to_str, parse_kind};
pub(crate) use super::helpers_calls_scan::InRamCallIndex;
use super::helpers_calls_scan::{CallRef, CallScanPage, budget_call_page};
use super::helpers_refs_match::Matching;
use super::index_route::IndexRoute;
use super::types::ReferenceHit;
use crate::index::view::Part;
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
    /// Resolve IN-PROCESS through the daemon's own workspace pool — the daemon-HOSTED path. The pool
    /// holds the workspace's read-write fjall index (the sole writer), so this serves the cross-file
    /// `refs_by_def` edges the hosted stack's index-less store cannot see, WITHOUT the daemon dialing
    /// itself over the socket (which the `Daemon` arm would do).
    #[cfg(all(feature = "comms", any(unix, windows)))]
    Host {
        /// The daemon's workspace pool, as the in-process host seam.
        host: std::sync::Arc<dyn crate::mcp::HostBackend>,
        /// Canonical workspace root, selecting the pool's hot workspace.
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
            #[cfg(all(feature = "comms", any(unix, windows)))]
            RefsSource::Host { host, root } => {
                use crate::comms::resolved_proto::{ResolvedRefQuery, ResolvedRefResult};
                let host = std::sync::Arc::clone(host);
                let root = root.clone();
                let query = ResolvedRefQuery::ReferencesTo {
                    def_path: def_path.clone(),
                    def_start,
                };
                // ~keep The fjall prefix scan is blocking, so it runs off the reactor; a host error degrades
                // ~keep to empty exactly like the `Daemon` arm, so `find_callers` still floors on the name scan.
                match tokio::task::spawn_blocking(move || host.host_resolved_refs(&root, query)).await {
                    Ok(Ok(ResolvedRefResult::References(uses))) => uses,
                    Ok(Ok(_)) => Vec::new(),
                    Ok(Err(error)) => {
                        tracing::debug!(%error, "find_callers: host resolved-refs failed; name-based fallback");
                        Vec::new()
                    }
                    Err(join) => {
                        tracing::debug!(%join, "find_callers: host resolved-refs task panicked; name-based fallback");
                        Vec::new()
                    }
                }
            }
        }
    }
}

/// Point-lookup a `Call` value in the index by `(path, start_byte)` and return its
/// `(line, column)` as 1-based / 0-based respectively. Falls back to `(0, 0)` when the
/// row/col fields aren't populated (older L2 blobs predating the field's introduction).
pub(super) fn resolve_call_line_col(
    idx: &dyn crate::index::view::IndexView,
    rel: &crate::path::RelPath,
    start_byte: u32,
) -> (u32, u32) {
    let key = crate::index::keys::call_by_path(rel, start_byte);
    let value = match idx.get(Part::CallsByPath, &key) {
        Ok(Some(v)) => v,
        _ => return (0, 0),
    };
    let call: crate::extract::Call = match rmp_serde::from_slice(&value) {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    (call.start_row + 1, call.start_col)
}

/// Body of the `code` tool's `references` mode. `route` decides where the `calls_by_callee` scan
/// runs; the caller releases the store lock before awaiting it.
pub(super) async fn run_find_references(
    route: &IndexRoute,
    params: super::types::FindReferencesParams,
    cache: &super::MapCache,
    notice: impl FnOnce() -> Option<super::types::LifecycleNotice>,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    use super::types::FindReferencesResponse;
    let format = super::toon::ResponseFormat::parse(params.format.as_deref());
    let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    let cursor_bytes = params.cursor.as_ref().map(|c| c.decode_fjall()).transpose()?;
    let matching = Matching::from_substring(params.substring);
    let scan = route
        .scan_calls(cache, &params.name, matching, limit, cursor_bytes.as_deref())
        .await?;
    let total = scan.total;
    let total_is_partial = scan.total_is_partial;
    let budgeted = budget_call_page(scan, params.max_tokens);
    super::toon::format_result(
        &FindReferencesResponse {
            name: params.name,
            matching: matching.as_str(),
            total,
            truncated: total_is_partial,
            truncated_reason: total_is_partial.then_some(super::types::SCAN_CAP),
            budgeted: budgeted.budgeted,
            hits: budgeted.hits,
            next_cursor: budgeted.next_cursor,
            notice: notice(),
            elapsed_us: super::helpers::elapsed_us(started),
        },
        format,
    )
}

/// Body of the `code` tool's `callers` mode.
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
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_find_callers(
    store: &crate::store::Store,
    refs: RefsSource<'_>,
    route: &IndexRoute,
    files: super::tool_contract::IndexedFiles<'_>,
    cache: &super::MapCache,
    params: super::types::FindCallersParams,
    notice: impl FnOnce() -> Option<super::types::LifecycleNotice>,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    use super::types::{DefinitionView, FindCallersResponse};
    let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    let kind_filter = params.kind.as_deref().map(parse_kind).transpose()?;
    // A pinned call that cannot be pinned is an error: answering with the unpinned name scan would
    // read as "these are the callers of the definition in that file".
    let not_indexed = || super::tool_contract::not_indexed_in(store, files.root, "code_refs", &params.path);
    // One snapshot for the stamp, the outline and the resolution facts: all three follow the
    // store's entry, which this call holds a read guard on.
    let (entry, l1) = super::freshness::indexed_outline(store, cache, &params.path).ok_or_else(not_indexed)?;
    let file = files.file(store, &params.path).map_err(|refusal| match refusal {
        super::tool_contract::Unreadable::NotIndexed => not_indexed(),
        super::tool_contract::Unreadable::Outside => {
            super::tool_contract::outside_repository("code_refs", &params.path)
        }
    })?;
    // The outline's spans and the resolution facts below are offsets into the indexed bytes.
    let source = super::index_route::blocking(|| {
        super::freshness::load_fresh("code_refs", &params.path, &file, (&entry).into(), files.max_file_bytes)
    })?;
    let symbol = super::helpers_pick::pick_symbol(
        "code_refs",
        &params.path,
        None,
        &l1.symbols,
        Some(&source),
        &params.name,
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

    let cursor_bytes = params.cursor.as_ref().map(|c| c.decode_fjall()).transpose()?;
    let matching = Matching::from_substring(params.substring);
    let scan = route
        .scan_calls(cache, &params.name, matching, limit, cursor_bytes.as_deref())
        .await?;

    // Resolution facts are a per-file blob the scan writes only for a file that produced some. No
    // blob is not `resolved_total: 0`: the language has no resolution engine, the resolve pass did
    // not finish, or this file bound nothing; `resolution` says which (`helpers_intel::resolution_gap`).
    let resolution_ran = store.read_resolved_by_hex(&entry.hash_hex).ok().flatten().is_some();
    let resolved = if resolution_ran {
        resolved_callers(
            store,
            &refs,
            route,
            cache,
            &params.path,
            &source,
            symbol,
            &params.name,
            limit,
        )
        .await
    } else {
        None
    };
    let resolved_total = resolved.as_ref().map(|r| r.total);
    let resolution = if resolved.is_some() {
        None
    } else if resolution_ran {
        Some(super::helpers_intel::NO_DEFINITION_FACTS.to_string())
    } else {
        super::helpers_intel::resolution_gap(
            cache.language_of(&params.path),
            super::helpers_intel::resolve_pass_complete(store),
        )
    };

    let page = merge_resolved(scan, resolved, limit, cursor_bytes.as_deref());
    let total = page.total;
    let total_is_partial = page.total_is_partial;
    let budgeted = budget_call_page(page, params.max_tokens);
    json_result(&FindCallersResponse {
        definition,
        matching: matching.as_str(),
        resolution,
        resolved_total,
        total,
        truncated: total_is_partial,
        truncated_reason: total_is_partial.then_some(super::types::SCAN_CAP),
        budgeted: budgeted.budgeted,
        hits: budgeted.hits,
        next_cursor: budgeted.next_cursor,
        notice: notice(),
        elapsed_us: super::helpers::elapsed_us(started),
    })
}

/// The scope/import-resolved callers of a definition — the refinement layer over the name scan.
///
/// Deliberately NOT a complete caller set: it is everything resolution can *prove*, which is a
/// subset of the truth whose gap resolution cannot measure. See [`run_find_callers`].
#[derive(Default)]
pub(super) struct ResolvedCallers {
    /// `path` → proven call-site start bytes. Probed per name-scan hit to set `resolved`.
    /// A map-of-sets rather than a set of `(RelPath, u32)` so probing borrows the hit's path
    /// instead of cloning it per hit on the scan's inner loop.
    pub(super) sites: ahash::AHashMap<crate::path::RelPath, ahash::AHashSet<u32>>,
    /// Files holding resolved uses that were past the probe cap: nothing is known about their call
    /// sites, so hits there stay unannotated.
    pub(super) unprobed: ahash::AHashSet<crate::path::RelPath>,
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

/// Build the resolved refinement for the definition `symbol` in `def_path`, whose indexed bytes
/// are `def_source`. `None` when resolution has nothing to say about that definition (its facts do
/// not name it, or the call sites could not be fetched): no hit is then marked either way. A
/// definition resolution knows and found no use of yields an empty refinement, which marks every
/// hit unproven. Cross-file uses come from the index via [`RefsSource`]: read locally when it is
/// open, else forwarded to the machine daemon on a `daemon_writer` serve.
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
#[allow(clippy::too_many_arguments)]
async fn resolved_callers(
    store: &crate::store::Store,
    ref_source: &RefsSource<'_>,
    route: &IndexRoute,
    cache: &MapCache,
    def_path: &crate::path::RelPath,
    def_source: &[u8],
    symbol: &crate::extract::Symbol,
    name: &str,
    limit: usize,
) -> Option<ResolvedCallers> {
    let entry = store.lookup(def_path)?;
    let refs = store.read_resolved_by_hex(&entry.hash_hex).ok().flatten()?;

    let mut def_starts: Vec<u32> = Vec::new();
    let push_candidate = |byte: u32, def_starts: &mut Vec<u32>| {
        if byte >= symbol.start_byte
            && byte < symbol.end_byte
            && super::helpers_intel::identifier_at(def_source, byte) == symbol.name.as_str()
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
        for use_ref in ref_source.references_to(def_path, def_start).await {
            if seen.insert(use_ref.clone()) {
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
    for batch in runs.chunks(CALL_SITE_BATCH) {
        let paths: Vec<crate::path::RelPath> = batch.iter().map(|(path, _)| (*path).clone()).collect();
        let per_file = route.calls_in_files(cache, &paths).await.ok()?;
        for ((use_path, run), calls) in batch.iter().zip(per_file) {
            let calls = index_by_start(calls);
            for (_, use_start) in *run {
                let Some(site) = calls.get(use_start) else {
                    continue;
                };
                total += 1;
                sites.entry((*use_path).clone()).or_default().insert(*use_start);
                if finder.find(site.callee.as_bytes()).is_none()
                    && let Some(key) = crate::index::keys::call_by_callee(&site.callee, use_path, *use_start)
                {
                    aliased.push((
                        key,
                        ReferenceHit {
                            path: (*use_path).clone(),
                            line: site.line,
                            column: site.column,
                            callee: site.callee.clone(),
                            resolved: Some(true),
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

/// Fold the resolved refinement into the name-scan page: annotate each scan hit with whether
/// resolution proved it, and merge in the aliased call sites the scan cannot see.
///
/// The merge is by `calls_by_callee` key — the same total order the scan iterates — so the emitted
/// page stays a contiguous prefix of one key-ordered stream and the cursor contract is unchanged:
/// `next_cursor` is the last key emitted, and the next call resumes strictly after it. Aliased hits
/// already emitted (key <= cursor) are filtered out, so paging never duplicates them.
pub(super) fn merge_resolved(
    mut page: CallScanPage,
    resolved: Option<ResolvedCallers>,
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
        let proven = resolved.sites.get(&hit.path).is_some_and(|s| s.contains(start));
        hit.resolved = Some(proven);
    }

    if resolved.aliased.is_empty() {
        return page;
    }
    let total = page.total.saturating_add(resolved.aliased.len() as u32);
    // A scan that stopped at its examined-keys cap has looked at nothing past `stop`: an aliased
    // hit beyond it belongs to a later page, and the cursor must stay at `stop` for that page to
    // continue the scan from there.
    let stop = page.stopped_after.take();
    let scan_had_more = page.next_cursor.is_some() && stop.is_none();

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
#[path = "helpers_calls_tests.rs"]
mod tests;
