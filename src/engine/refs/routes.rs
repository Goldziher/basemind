//! Route linking for `code_refs` (ADR-0048): an HTTP route is a `Route` symbol named
//! `"GET /users/:param"`, a client request with a literal URL is a call row whose callee has the same
//! shape, and the two are linked by method and normalized path at query time.
//!
//! A request matches a route when the methods agree (a route of method `ANY` takes every method) and
//! the paths do: segment for segment (exact), or else one path is a whole-segment suffix of the
//! other (a client whose base URL holds a prefix, or a router mounted in another file). A route's
//! `:param` matches any one segment and a trailing `*` the rest of the path; a request's `:param`
//! (a value it builds) matches only a parameter. A route ending in `*` also matches inside a longer
//! request (`/static/*` against `/cdn/static/css/a.css`), as a suffix match does.
//!
//! A request belongs to the routes that match it best: a match through a literal segment before one
//! through parameters and wildcards alone, so a catch-all (`GET /*`, `GET /:slug`) never takes a
//! request from the route that names its path; then exact before suffix, more matched segments,
//! more literal segments, and the method itself before `ANY`. When several routes tie for a
//! request, its row is `ambiguous`: two services may expose one path, and the path alone cannot tell
//! which one answers. URLs built from variables never became call rows, so they are out of reach.

use super::calls::index_for_refs;
use super::calls_scan::{CallScanPage, Kept, SCAN_KEYS_EXAMINED_MAX, scan_calls_where};
pub(crate) use super::route_table::{RouteDef, Routes};
use crate::engine::MapCache;
use crate::engine::error::EngineError;
use crate::engine::files::{indexed_file, not_indexed_in};
use crate::engine::freshness::load_fresh;
use crate::engine::scope::ChangeScope;
use crate::engine::types::refs::{DefinitionLocation, GotoDefinitionResponse, ReferenceHit, Resolution};
use crate::engine::workspace::Workspace;
use crate::index::view::IndexView;
use crate::path::RelPath;

/// A path parameter, as both sides normalize it.
const PARAM: &str = ":param";

/// The tail wildcard of a route (`/static/*filepath` normalizes to `/static/*`).
const WILDCARD: &str = "*";

/// Every route name and client callee holds this between its method and its path; the call scan
/// reads only callees that do.
const METHOD_PATH_SEPARATOR: &str = " /";

/// The methods a route or request is named with; `ANY` is a route answering every method.
const METHODS: [&str; 8] = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "ANY"];

/// An HTTP endpoint as routes and client calls name it: a method and normalized path segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub(super) method: &'static str,
    pub(super) segments: Vec<String>,
}

impl Endpoint {
    /// `METHOD /path`, the method in any case; the path is normalized the way extraction
    /// normalizes it (`{id}` and `<id>` become `:param`). `None` for any other name.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        let (method, path) = name.split_once(' ')?;
        let method = METHODS.into_iter().find(|known| known.eq_ignore_ascii_case(method))?;
        if !path.starts_with('/') {
            return None;
        }
        let path = crate::extract::http::normalize_path(path, false)?;
        Some(Self {
            method,
            segments: path
                .split('/')
                .filter(|seg| !seg.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }

    /// The name as a route symbol or a client callee writes it.
    pub(crate) fn name(&self) -> String {
        let mut out = format!("{} ", self.method);
        if self.segments.is_empty() {
            out.push('/');
        }
        for seg in &self.segments {
            out.push('/');
            out.push_str(seg);
        }
        out
    }

    /// Whether the path ends in the wildcard.
    pub(super) fn wildcard_tail(&self) -> bool {
        self.segments.last().is_some_and(|seg| seg == WILDCARD)
    }
}

/// Whether a callee is an HTTP request (`GET /users/42`), without normalizing its path.
pub(crate) fn is_http_callee(callee: &str) -> bool {
    callee
        .split_once(' ')
        .is_some_and(|(method, path)| path.starts_with('/') && METHODS.contains(&method))
}

/// Whether a segment matches only its own text: no parameter, not the wildcard.
pub(super) fn is_literal(segment: &str) -> bool {
    segment != WILDCARD && !segment.contains(PARAM)
}

/// How well a route matches a request; a larger score is a better match. Field order is rank order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Score {
    /// A literal segment matched: a match through parameters and wildcards alone ranks below any
    /// match that names a segment of the path.
    has_literal: bool,
    /// Every segment matched (else one path is a suffix of the other).
    exact: bool,
    /// Segments compared and matched.
    segments: usize,
    /// Of those, segments equal as literal text.
    literals: usize,
    /// The route names the request's method (else it is `ANY`).
    same_method: bool,
}

impl Score {
    fn new(exact: bool, segments: usize, literals: usize, same_method: bool) -> Self {
        Self {
            has_literal: literals > 0,
            exact,
            segments,
            literals,
            same_method,
        }
    }
}

/// The score of `route` for the request `call`, or `None` when it does not match.
pub(crate) fn score(route: &Endpoint, call: &Endpoint) -> Option<Score> {
    let same_method = route.method == call.method;
    if !same_method && route.method != "ANY" {
        return None;
    }
    let (r, c) = (&route.segments, &call.segments);
    let wildcard_tail = route.wildcard_tail();
    if (r.len() == c.len() || (wildcard_tail && c.len() > r.len()))
        && let Some(literals) = segments_match(r, &c[..r.len()])
    {
        return Some(Score::new(true, r.len(), literals, same_method));
    }
    if wildcard_tail && c.len() > r.len() {
        // The route's prefix inside the request, after a base URL the client holds.
        return (1..=c.len() - r.len())
            .find_map(|from| segments_match(r, &c[from..from + r.len()]))
            .filter(|literals| *literals > 0)
            .map(|literals| Score::new(false, r.len(), literals, same_method));
    }
    let shared = r.len().min(c.len());
    if r.len() == c.len() || shared == 0 {
        return None;
    }
    let literals = segments_match(&r[r.len() - shared..], &c[c.len() - shared..])?;
    (literals > 0).then_some(Score::new(false, shared, literals, same_method))
}

/// The literal segments matched when every route segment of `r` matches its request segment of
/// `c` (same length); `None` at the first that does not.
fn segments_match(r: &[String], c: &[String]) -> Option<usize> {
    let mut literals = 0;
    for (route_seg, call_seg) in r.iter().zip(c) {
        literals += usize::from(segment_matches(route_seg, call_seg)?);
    }
    Some(literals)
}

/// Whether one route segment matches one request segment: `Some(true)` as equal literal text,
/// `Some(false)` through a parameter or the wildcard, `None` when it does not match.
fn segment_matches(route: &str, call: &str) -> Option<bool> {
    if route == call {
        return Some(is_literal(route));
    }
    if route == PARAM || route == WILDCARD {
        return Some(false);
    }
    // A parameter inside a segment (`:param.json`) takes any text between its fixed parts.
    let (before, after) = route.split_once(PARAM)?;
    (call.len() >= before.len() + after.len() && call.starts_with(before) && call.ends_with(after)).then_some(false)
}

impl Routes {
    /// The routes that match the request `call` best, in path order.
    pub(crate) fn matching(&self, call: &Endpoint) -> Vec<&RouteDef> {
        let scored: Vec<(Score, &RouteDef)> = self
            .candidates(call)
            .filter_map(|route| Some((score(&route.endpoint, call)?, route)))
            .collect();
        let Some(best) = scored.iter().map(|(score, _)| *score).max() else {
            return Vec::new();
        };
        let mut out: Vec<&RouteDef> = scored
            .into_iter()
            .filter(|(score, _)| *score == best)
            .map(|(_, route)| route)
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
        out
    }
}

/// Decides which requests are calls to one endpoint: those it matches at least as well as every
/// indexed route does. Memoized per callee, since a scan meets each request path many times.
pub(crate) struct RouteLinker<'r> {
    routes: &'r Routes,
    target: Endpoint,
    /// No indexed route is the target (a `METHOD /path` name no route has): the target is one more
    /// endpoint a request can tie with.
    unindexed: bool,
    seen: ahash::AHashMap<String, Option<Kept>>,
}

impl<'r> RouteLinker<'r> {
    pub(crate) fn new(routes: &'r Routes, target: Endpoint) -> Self {
        Self {
            unindexed: !routes.holds(&target),
            routes,
            target,
            seen: ahash::AHashMap::new(),
        }
    }

    /// How the request `callee` is listed among the target's callers, or `None` when it is not
    /// one: it does not match the target, or another route matches it better. It is `ambiguous`
    /// when more than one endpoint, indexed or the target, matches it as well as the target does.
    pub(crate) fn keep(&mut self, callee: &str) -> Option<Kept> {
        if let Some(kept) = self.seen.get(callee) {
            return *kept;
        }
        let kept = self.judge(callee);
        self.seen.insert(callee.to_owned(), kept);
        kept
    }

    fn judge(&self, callee: &str) -> Option<Kept> {
        let call = Endpoint::parse(callee)?;
        let own = score(&self.target, &call)?;
        let mut tied = usize::from(self.unindexed);
        for route in self.routes.candidates(&call) {
            match score(&route.endpoint, &call) {
                Some(other) if other > own => return None,
                Some(other) if other == own => tied += 1,
                _ => {}
            }
        }
        Some(Kept {
            resolution: Resolution::PathMatch,
            ambiguous: tied > 1,
        })
    }
}

/// The scan behind route callers: every client request in `scope` linked to `target`, with the
/// generation of the key order it was cut in (as [`super::calls_scan::scan_calls`]).
pub(crate) fn scan_route_calls(
    idx: &dyn IndexView,
    cache: &MapCache,
    target: &Endpoint,
    limit: usize,
    cursor_after: Option<&[u8]>,
    scope: Option<&ChangeScope>,
) -> Result<(CallScanPage, u64), EngineError> {
    let generation = idx.live_keys().fingerprint();
    let routes = cache.routes();
    let mut linker = RouteLinker::new(&routes, target.clone());
    let page = scan_calls_where(
        idx,
        METHOD_PATH_SEPARATOR,
        limit,
        cursor_after,
        SCAN_KEYS_EXAMINED_MAX,
        |callee, path| {
            if crate::engine::changed::admits(scope, path) {
                linker.keep(callee)
            } else {
                None
            }
        },
    )?;
    Ok((page, generation))
}

/// HTTP calls linked to one handler's routes that a page of its callers reads at most. Past it the
/// answer is `truncated` (`scan_cap`).
const HANDLER_CALLS_MAX: usize = 2_000;

/// What the callers of a handler definition gain (ADR-0048 decision 5): the routes that name it and
/// the HTTP calls linked to those routes, each graded `path_match` and keyed by its place in the
/// `calls_by_callee` order, so it merges into the name scan's pages like an aliased call site.
pub(crate) struct HandlerLinks {
    pub(crate) hits: Vec<(Vec<u8>, ReferenceHit)>,
    /// The linked calls were cut at [`HANDLER_CALLS_MAX`] or the scan's examined-keys cap.
    pub(crate) partial: bool,
}

/// The routes naming the definition `name` in `def_path` and the HTTP calls that reach them; `None`
/// when no route names it.
///
/// A route names its handler by text (`get(show_user)`, `users.show`), compared by its last
/// component. A route in the definition's file names it; a route in another file may name another
/// definition of the name, so its rows are `ambiguous` when more than one file defines the name.
///
/// A call the name scan lists already (`listed`: a `substring` scan holding the name) is left to
/// it, so no site is counted twice.
pub(crate) fn handler_links(
    idx: &dyn IndexView,
    cache: &MapCache,
    def_path: &RelPath,
    name: &str,
    scope: Option<&ChangeScope>,
    listed: impl Fn(&str) -> bool,
) -> Result<Option<HandlerLinks>, EngineError> {
    let routes = cache.routes();
    let named: Vec<&RouteDef> = routes.naming(name).collect();
    if named.is_empty() {
        return Ok(None);
    }
    let shared_name = named.iter().any(|route| route.path != *def_path) && files_defining(idx, name) > 1;
    let live = idx.live_keys();
    let mut hits = Vec::new();
    let mut linkers: Vec<(RouteLinker<'_>, bool)> = Vec::with_capacity(named.len());
    for route in named {
        let ambiguous = shared_name && route.path != *def_path;
        if !linkers.iter().any(|(linker, _)| linker.target == route.endpoint) {
            linkers.push((RouteLinker::new(&routes, route.endpoint.clone()), ambiguous));
        }
        if !crate::engine::changed::admits(scope, &route.path) {
            continue;
        }
        let Some(key) = live.key_of(&route.path).and_then(|content| {
            crate::index::keys::call_by_callee_hit(&route.name, content, route.start_byte, &route.path)
        }) else {
            continue;
        };
        hits.push((
            key,
            ReferenceHit {
                path: route.path.clone(),
                line: route.line,
                column: route.column,
                callee: route.name.clone(),
                resolution: Resolution::PathMatch,
                elide_resolution: false,
                shared_line: false,
                stale: false,
                ambiguous,
            },
        ));
    }
    let page = scan_calls_where(
        idx,
        METHOD_PATH_SEPARATOR,
        HANDLER_CALLS_MAX,
        None,
        SCAN_KEYS_EXAMINED_MAX,
        |callee, path| {
            if listed(callee) || !crate::engine::changed::admits(scope, path) {
                return None;
            }
            linkers.iter_mut().find_map(|(linker, ambiguous)| {
                linker.keep(callee).map(|kept| Kept {
                    ambiguous: kept.ambiguous || *ambiguous,
                    ..kept
                })
            })
        },
    )?;
    let partial = page.total_is_partial || page.next_cursor.is_some();
    hits.extend(page.hit_keys.into_iter().zip(page.hits));
    Ok(Some(HandlerLinks { hits, partial }))
}

/// Files that define a symbol named `name`, counted up to a few.
fn files_defining(idx: &dyn IndexView, name: &str) -> usize {
    let mut files: Vec<RelPath> = idx
        .symbols_by_name_lookup(name, 64)
        .into_iter()
        .filter(|(found, ..)| found == name)
        .map(|(_, _, path, _)| path)
        .collect();
    files.sort_unstable();
    files.dedup();
    files.len()
}

/// HTTP calls one file holds that a definition lookup lists when the name does not fit the line.
const FILE_REQUESTS_MAX: usize = 1_000;

/// `code_refs` relation `definition` for an HTTP request: the route the call named `call` on
/// `line` of `path` reaches. The call must be on that line; the column plays no part, since the
/// line and the name pick the call. The line is a line of the indexed file, so a file edited since
/// the scan is refused, as for any other reference.
///
/// The calls come from the callee scan, not the file's call rows: a request and the chained call
/// around it (`client.get(..).send()`) start at one byte, and the file's rows keep one call per
/// start byte. The scan reads the callee `call` names; only when the line holds no such call does it
/// read every HTTP callee, to name the calls the line does hold.
pub(crate) fn run_route_definition(
    ws: &Workspace,
    path: RelPath,
    line: u32,
    call: &Endpoint,
) -> Result<GotoDefinitionResponse, EngineError> {
    ws.wait_map_ready();
    let cache = ws.cache.load_full();
    let name = call.name();
    let site = {
        let store = ws.store_read();
        let Some(entry) = store.lookup(&path) else {
            return Err(not_indexed_in(&store, &ws.root, "code_refs", &path));
        };
        let file = indexed_file(ws, &store, "code_refs", &path)?;
        load_fresh("code_refs", &path, &file, entry.into(), ws.config.scan.max_file_bytes)?;
        let idx = index_for_refs(&store)?;
        let exact = scan_calls_where(
            &*idx,
            &name,
            FILE_REQUESTS_MAX,
            None,
            SCAN_KEYS_EXAMINED_MAX,
            |callee, site_path| (callee == name && *site_path == path).then_some(Kept::NAME_ONLY),
        )?;
        match exact.hits.into_iter().find(|site| site.line == line) {
            Some(site) => site,
            None => return Err(no_call_on_line(&*idx, &path, line, &name)?),
        }
    };
    let routes = cache.routes();
    let matched = routes.matching(call);
    let located = |route: &&RouteDef| DefinitionLocation {
        path: route.path.clone(),
        line: route.line,
        column: route.column + 1,
        name: route.name.clone(),
    };
    let (definition, ambiguous) = match matched.as_slice() {
        [one] => (Some(located(one)), Vec::new()),
        many => (None, many.iter().map(located).collect()),
    };
    Ok(GotoDefinitionResponse {
        path,
        line,
        column: site.column + 1,
        definition,
        resolution_note: matched
            .is_empty()
            .then(|| "no indexed route matches this call's method and path".to_string()),
        resolution: (!matched.is_empty()).then_some(Resolution::PathMatch),
        ambiguous,
    })
}

/// The refusal for a `definition` lookup whose line holds no HTTP call named `name`: it names the
/// HTTP calls the line does hold, or says the line holds none, or that the scan stopped before it
/// could tell.
fn no_call_on_line(idx: &dyn IndexView, path: &RelPath, line: u32, name: &str) -> Result<EngineError, EngineError> {
    let requests = scan_calls_where(
        idx,
        METHOD_PATH_SEPARATOR,
        FILE_REQUESTS_MAX,
        None,
        SCAN_KEYS_EXAMINED_MAX,
        |callee, site_path| (site_path == path && Endpoint::parse(callee).is_some()).then_some(Kept::NAME_ONLY),
    )?;
    let listed: Vec<String> = requests
        .hits
        .iter()
        .filter(|site| site.line == line)
        .map(|site| format!("`{}`", site.callee))
        .collect();
    let message = if !listed.is_empty() {
        format!(
            "`code_refs`: the HTTP call on line {line} of `{path}` is {}, not `{name}`; pass it as `name`",
            listed.join(", ")
        )
    } else if requests.total_is_partial || requests.next_cursor.is_some() {
        format!(
            "`code_refs`: no HTTP call `{name}` on line {line} of `{path}`, and the scan of the repository's HTTP \
             calls stopped at its cap before it reached that line; pass the call's method and path as `name`"
        )
    } else {
        format!(
            "`code_refs`: no HTTP call with a literal URL on line {line} of `{path}`; a URL built from variables is \
             out of reach"
        )
    };
    Ok(EngineError::invalid_argument(message))
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod tests;
