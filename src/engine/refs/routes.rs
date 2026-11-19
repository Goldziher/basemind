//! Route linking for `code_refs` (ADR-0048): an HTTP route is a `Route` symbol named
//! `"GET /users/:param"`, a client request with a literal URL is a call row whose callee has the same
//! shape, and the two are linked by method and normalized path at query time.
//!
//! A request matches a route when the methods agree (a route of method `ANY` takes every method) and
//! the paths do: segment for segment (exact), or else one path is a whole-segment suffix of the
//! other (a client whose base URL holds a prefix, or a router mounted in another file). A route's
//! `:param` matches any one segment and a trailing `*` the rest of the path; a request's `:param`
//! (a value it builds) matches only a parameter. A request belongs to the routes that match it best:
//! exact before suffix, then more matched segments, more literal segments, and the method itself
//! before `ANY`. When several routes tie for a request, its row is `ambiguous`: two services may
//! expose one path, and the path alone cannot tell which one answers. URLs built from variables
//! never became call rows, so they are out of reach.

use super::calls::index_for_refs;
use super::calls_scan::{CallScanPage, Kept, SCAN_KEYS_EXAMINED_MAX, scan_calls_where};
use crate::engine::MapCache;
use crate::engine::error::EngineError;
use crate::engine::files::not_indexed_in;
use crate::engine::scope::ChangeScope;
use crate::engine::types::refs::{DefinitionLocation, GotoDefinitionResponse, Resolution};
use crate::engine::workspace::Workspace;
use crate::extract::SymbolKind;
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

/// Routes a query reads at most. Past it the table is cut, so a request may link to fewer routes.
const ROUTES_MAX: usize = 20_000;

/// An HTTP endpoint as routes and client calls name it: a method and normalized path segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    method: &'static str,
    segments: Vec<String>,
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
}

/// How well a route matches a request; a larger score is a better match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Score {
    /// Every segment matched (else one path is a suffix of the other).
    exact: bool,
    /// Segments compared and matched.
    segments: usize,
    /// Of those, segments equal as literal text.
    literals: usize,
    /// The route names the request's method (else it is `ANY`).
    same_method: bool,
}

/// The score of `route` for the request `call`, or `None` when it does not match.
pub(crate) fn score(route: &Endpoint, call: &Endpoint) -> Option<Score> {
    let same_method = route.method == call.method;
    if !same_method && route.method != "ANY" {
        return None;
    }
    let (r, c) = (&route.segments, &call.segments);
    let wildcard_tail = r.last().is_some_and(|seg| seg == WILDCARD);
    if (r.len() == c.len() || (wildcard_tail && c.len() > r.len()))
        && let Some(literals) = segments_match(r, &c[..r.len()])
    {
        return Some(Score {
            exact: true,
            segments: r.len(),
            literals,
            same_method,
        });
    }
    let shared = r.len().min(c.len());
    if r.len() == c.len() || shared == 0 {
        return None;
    }
    let literals = segments_match(&r[r.len() - shared..], &c[c.len() - shared..])?;
    (literals > 0).then_some(Score {
        exact: false,
        segments: shared,
        literals,
        same_method,
    })
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
        return Some(!route.contains(PARAM) && route != WILDCARD);
    }
    if route == PARAM || route == WILDCARD {
        return Some(false);
    }
    // A parameter inside a segment (`:param.json`) takes any text between its fixed parts.
    let (before, after) = route.split_once(PARAM)?;
    (call.len() >= before.len() + after.len() && call.starts_with(before) && call.ends_with(after)).then_some(false)
}

/// One indexed route.
#[derive(Debug, Clone)]
pub(crate) struct RouteDef {
    pub(crate) path: RelPath,
    pub(crate) name: String,
    endpoint: Endpoint,
    /// 1-based line of the route registration.
    pub(crate) line: u32,
    /// 0-based byte column.
    pub(crate) column: u32,
}

/// Every route the index holds, read from the resident term index; positions come from the
/// outlines of the files that hold routes.
pub(crate) struct Routes(Vec<RouteDef>);

impl Routes {
    pub(crate) fn of(cache: &MapCache) -> Self {
        let finder = memchr::memmem::Finder::new(METHOD_PATH_SEPARATOR.as_bytes());
        let mut out = Vec::new();
        for (path, terms) in cache.terms().iter() {
            let indices: Vec<usize> = terms.matching_symbols(&finder, Some(SymbolKind::Route)).collect();
            if indices.is_empty() {
                continue;
            }
            let Some(l1) = cache.get(path) else {
                continue;
            };
            for symbol in indices.into_iter().filter_map(|idx| l1.symbols.get(idx)) {
                if out.len() >= ROUTES_MAX {
                    return Self(out);
                }
                let Some(endpoint) = Endpoint::parse(&symbol.name) else {
                    continue;
                };
                out.push(RouteDef {
                    path: path.clone(),
                    name: symbol.name.clone(),
                    endpoint,
                    line: symbol.start_row + 1,
                    column: symbol.start_col,
                });
            }
        }
        Self(out)
    }

    #[cfg(test)]
    pub(crate) fn from_names(names: &[(&str, &str)]) -> Self {
        Self(
            names
                .iter()
                .enumerate()
                .map(|(line, (path, name))| RouteDef {
                    path: RelPath::from(*path),
                    name: (*name).to_string(),
                    endpoint: Endpoint::parse(name).expect("a route name"),
                    line: line as u32 + 1,
                    column: 0,
                })
                .collect(),
        )
    }

    /// The routes that match the request `call` best, in path order.
    pub(crate) fn matching(&self, call: &Endpoint) -> Vec<&RouteDef> {
        let scored: Vec<(Score, &RouteDef)> = self
            .0
            .iter()
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
pub(crate) struct RouteLinker {
    routes: Routes,
    target: Endpoint,
    seen: ahash::AHashMap<String, Option<Kept>>,
}

impl RouteLinker {
    pub(crate) fn new(routes: Routes, target: Endpoint) -> Self {
        Self {
            routes,
            target,
            seen: ahash::AHashMap::new(),
        }
    }

    /// How the request `callee` is listed among the target's callers, or `None` when it is not
    /// one: it does not match the target, or another route matches it better. It is `ambiguous`
    /// when more than one indexed route matches it as well as the target does.
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
        let mut tied = 0usize;
        for route in &self.routes.0 {
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
    let mut linker = RouteLinker::new(Routes::of(cache), target.clone());
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

/// HTTP calls one file holds that a definition lookup lists when the name does not fit the line.
const FILE_REQUESTS_MAX: usize = 1_000;

/// `code_refs` relation `definition` for an HTTP request: the route the call named `call` on
/// `line` of `path` reaches. The call must be on that line; the column plays no part, since the
/// line and the name pick the call.
///
/// The calls come from the callee scan, not the file's call rows: a request and the chained call
/// around it (`client.get(..).send()`) start at one byte, and the file's rows keep one call per
/// start byte.
pub(crate) fn run_route_definition(
    ws: &Workspace,
    path: RelPath,
    line: u32,
    call: &Endpoint,
) -> Result<GotoDefinitionResponse, EngineError> {
    ws.wait_map_ready();
    let cache = ws.cache.load_full();
    let requests = {
        let store = ws.store_read();
        if store.lookup(&path).is_none() {
            return Err(not_indexed_in(&store, &ws.root, "code_refs", &path));
        }
        let idx = index_for_refs(&store)?;
        scan_calls_where(
            &*idx,
            METHOD_PATH_SEPARATOR,
            FILE_REQUESTS_MAX,
            None,
            SCAN_KEYS_EXAMINED_MAX,
            |callee, site_path| (*site_path == path && Endpoint::parse(callee).is_some()).then_some(Kept::NAME_ONLY),
        )?
        .hits
    };
    let name = call.name();
    let on_line: Vec<_> = requests.iter().filter(|site| site.line == line).collect();
    let Some(site) = on_line.iter().find(|site| site.callee == name) else {
        let listed: Vec<String> = on_line.iter().map(|site| format!("`{}`", site.callee)).collect();
        return Err(EngineError::invalid_argument(if listed.is_empty() {
            format!(
                "`code_refs`: no HTTP call with a literal URL on line {line} of `{path}`; a URL built from \
                 variables is out of reach"
            )
        } else {
            format!(
                "`code_refs`: the HTTP call on line {line} of `{path}` is {}, not `{name}`; pass it as `name`",
                listed.join(", ")
            )
        }));
    };
    let routes = Routes::of(&cache);
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

#[cfg(test)]
#[path = "routes_tests.rs"]
mod tests;
