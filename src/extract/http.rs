//! HTTP routes and literal-URL client calls (ADR-0048).
//!
//! A `;; section: routes` match becomes a [`SymbolKind::Route`] symbol named `"GET /users/:param"`
//! whose signature is the handler; a `;; section: http_calls` match becomes a [`Call`] whose callee
//! is the same method-and-path string, so a route and the requests to it compare equal by name.
//!
//! Capture vocabulary. Routes: `@route.range` (the span), `@route.anchor` (where the span starts
//! instead, for a chained `.route(..)`), `@route.method` (a verb word: `get`, `GET`, `GetMapping`,
//! Flask `route`), `@route.path`, `@route.handler`, `@route.receiver` (the router the route is
//! registered on), `@route.args` (the decorator or annotation arguments, for `methods=`, `method =`,
//! `value =`) and `@route.method_router` (an axum `get(h).post(h2)` chain). Mounts, which prefix the
//! routes of one router or one scope: `@mount.name` (the router mounted), `@mount.parent` (the router
//! it is mounted on), `@mount.path` or `@mount.args`, and `@mount.scope` (routes inside the node take
//! the prefix). Clients: `@http.range`, `@http.method` (a verb word, or `fetch` / `axios` /
//! `request` / `NewRequest*`), `@http.receiver` and `@http.args`.

use tree_sitter::{Node, Query, QueryMatch};

use super::{Call, Symbol, SymbolKind, capture_name};

/// What every path parameter normalizes to (`:id`, `{id}`, `<int:id>`, `${id}`), so a route and a
/// call to it compare equal whatever each side names the parameter.
const PARAM: &str = ":param";

/// Stands in for a parameter between substitution and segment normalization.
const SENTINEL: char = '\u{1}';

/// The method of a route that answers every method (`app.all`, gin `Any`, a bare `@RequestMapping`).
const ANY: &str = "ANY";

/// Mount chains deeper than this are cut; also ends a cycle (`a.use("/x", b); b.use("/y", a)`).
const MAX_MOUNT_DEPTH: usize = 8;

/// Receivers whose calls are HTTP requests whatever the URL. Through any other receiver (a client
/// instance, a map) a call counts only when its URL is an absolute `http(s)://` literal.
const CLIENT_MODULES: &[&str] = &["axios", "requests", "httpx", "reqwest", "http"];

/// Node kinds of a string literal across the grammars with route sections.
const LITERAL_KINDS: &[&str] = &[
    "string",
    "template_string",
    "string_literal",
    "raw_string_literal",
    "interpreted_string_literal",
];

/// Node kinds a route's handler can have: a name, an inline function, or a call that returns one.
/// A call-style registration whose last argument is anything else (`{ params }`) is a request.
const CALLABLE_KINDS: &[&str] = &[
    "identifier",
    "member_expression",
    "selector_expression",
    "arrow_function",
    "function_expression",
    "function",
    "func_literal",
    "call_expression",
];

/// Node kinds that name a handler; an inline closure or function has no name to record.
const HANDLER_KINDS: &[&str] = &[
    "identifier",
    "member_expression",
    "selector_expression",
    "attribute",
    "field_expression",
    "scoped_identifier",
    "field_access",
];

/// Routes and mounts collected from one file's L1 walk; [`Routes::finish`] folds the mounts in.
#[derive(Default)]
pub(super) struct Routes {
    routes: Vec<RawRoute>,
    mounts: Vec<Mount>,
}

struct RawRoute {
    method: &'static str,
    path: String,
    handler: Option<String>,
    receiver: Option<String>,
    start: Span,
}

/// The position fields of a [`Symbol`], copied out of the nodes so no tree borrow outlives the walk.
#[derive(Clone, Copy)]
struct Span {
    start_byte: u32,
    end_byte: u32,
    start_row: u32,
    start_col: u32,
}

struct Mount {
    name: Option<String>,
    parent: Option<String>,
    scope: Option<(usize, usize)>,
    path: String,
}

#[derive(Default)]
struct RouteCaps<'t> {
    range: Option<Node<'t>>,
    anchor: Option<Node<'t>>,
    method: Option<Node<'t>>,
    path: Option<Node<'t>>,
    handler: Option<Node<'t>>,
    receiver: Option<Node<'t>>,
    args: Option<Node<'t>>,
    method_router: Option<Node<'t>>,
    mount_name: Option<Node<'t>>,
    mount_parent: Option<Node<'t>>,
    mount_path: Option<Node<'t>>,
    mount_args: Option<Node<'t>>,
    mount_scope: Option<Node<'t>>,
}

impl Routes {
    /// Record one match of the routes section: a route, or a mount that prefixes routes.
    pub(super) fn push_match(&mut self, q: &Query, m: &QueryMatch, source: &[u8]) {
        let mut c = RouteCaps::default();
        for cap in m.captures() {
            let slot = match capture_name(q, cap.index) {
                "route.range" => &mut c.range,
                "route.anchor" => &mut c.anchor,
                "route.method" => &mut c.method,
                "route.path" => &mut c.path,
                "route.handler" => &mut c.handler,
                "route.receiver" => &mut c.receiver,
                "route.args" => &mut c.args,
                "route.method_router" => &mut c.method_router,
                "mount.name" => &mut c.mount_name,
                "mount.parent" => &mut c.mount_parent,
                "mount.path" => &mut c.mount_path,
                "mount.args" => &mut c.mount_args,
                "mount.scope" => &mut c.mount_scope,
                _ => continue,
            };
            *slot = Some(cap.node);
        }
        if c.range.is_none() {
            self.push_mount(&c, source);
        } else {
            self.push_route(&c, source);
        }
    }

    fn push_mount(&mut self, c: &RouteCaps, source: &[u8]) {
        let path = c
            .mount_path
            .and_then(|n| literal(n, source))
            .or_else(|| c.mount_args.and_then(|a| args_path(a, source)));
        let Some(path) = path else { return };
        if c.mount_name.is_none() && c.mount_scope.is_none() {
            return;
        }
        self.mounts.push(Mount {
            name: c.mount_name.and_then(|n| text(n, source)).map(str::to_owned),
            parent: c.mount_parent.and_then(|n| text(n, source)).map(str::to_owned),
            scope: c.mount_scope.map(|n| (n.start_byte(), n.end_byte())),
            path: path.to_owned(),
        });
    }

    fn push_route(&mut self, c: &RouteCaps, source: &[u8]) {
        let Some(range) = c.range else { return };
        let from = c.anchor.unwrap_or(range);
        let p = from.start_position();
        let start = Span {
            start_byte: from.start_byte() as u32,
            end_byte: range.end_byte() as u32,
            start_row: p.row as u32,
            start_col: p.column as u32,
        };
        let path = match c.path {
            Some(n) => literal(n, source),
            None => Some(c.args.and_then(|a| args_path(a, source)).unwrap_or("")),
        };
        let Some(path) = path else { return };
        if c.handler.is_some_and(|h| !CALLABLE_KINDS.contains(&h.kind())) {
            return;
        }
        let receiver = c.receiver.and_then(|n| text(n, source)).map(str::to_owned);
        let mut pairs: Vec<(&'static str, Option<String>)> = Vec::new();
        if let Some(router) = c.method_router {
            method_router(router, source, &mut pairs);
        } else if let Some(word) = c.method.and_then(|n| text(n, source)) {
            let handler = c.handler.and_then(|n| handler_name(n, source));
            for method in route_methods(word, c.args, source) {
                pairs.push((method, handler.clone()));
            }
        }
        for (method, handler) in pairs {
            self.routes.push(RawRoute {
                method,
                path: path.to_owned(),
                handler,
                receiver: receiver.clone(),
                start,
            });
        }
    }

    /// The route symbols, each path prefixed by the literal mounts of its router and scope.
    pub(super) fn finish(mut self) -> Vec<Symbol> {
        // Query matches on a chain (`Router::new().route(..).route(..)`) arrive outermost first.
        self.routes.sort_by_key(|r| r.start.start_byte);
        let mut out = Vec::with_capacity(self.routes.len());
        for r in &self.routes {
            let mut full = String::new();
            for m in self.mounts.iter().filter(|m| {
                m.scope
                    .is_some_and(|(s, e)| (s..e).contains(&(r.start.start_byte as usize)))
            }) {
                full.push('/');
                full.push_str(&m.path);
            }
            if let Some(recv) = r.receiver.as_deref() {
                self.mount_prefix(recv, 0, &mut full);
            }
            full.push('/');
            full.push_str(&r.path);
            let Some(path) = normalize_path(&full, false) else {
                continue;
            };
            out.push(Symbol {
                name: format!("{} {path}", r.method),
                kind: SymbolKind::Route,
                start_byte: r.start.start_byte,
                end_byte: r.start.end_byte,
                start_row: r.start.start_row,
                start_col: r.start.start_col,
                signature: r.handler.clone(),
                decorators: Vec::new(),
            });
        }
        out
    }

    /// Append the prefix router `name` is mounted at: where it is mounted on another router
    /// (`app.use("/api", router)`, outermost first), then its own constructor prefix
    /// (`APIRouter(prefix="/users")`).
    fn mount_prefix(&self, name: &str, depth: usize, out: &mut String) {
        if depth >= MAX_MOUNT_DEPTH {
            return;
        }
        let named = || self.mounts.iter().filter(|m| m.name.as_deref() == Some(name));
        if let Some(outer) = named().find(|m| m.parent.is_some()) {
            if let Some(parent) = outer.parent.as_deref().filter(|p| *p != name) {
                self.mount_prefix(parent, depth + 1, out);
            }
            out.push('/');
            out.push_str(&outer.path);
        }
        for inner in named().filter(|m| m.parent.is_none()) {
            out.push('/');
            out.push_str(&inner.path);
        }
    }
}

/// A client call row for one match of the `http_calls` section, or `None` when the URL is not a
/// literal with at least one literal path segment, or the receiver is not a known client and the
/// URL is not absolute.
pub(super) fn client_call(q: &Query, m: &QueryMatch, source: &[u8]) -> Option<Call> {
    let (mut range, mut word, mut receiver, mut args) = (None, None, None, None);
    let mut word_node = None;
    for cap in m.captures() {
        match capture_name(q, cap.index) {
            "http.range" => range = Some(cap.node),
            "http.method" => {
                word = text(cap.node, source);
                word_node = Some(cap.node);
            }
            "http.receiver" => receiver = text(cap.node, source),
            "http.args" => args = Some(cap.node),
            _ => {}
        }
    }
    let (range, word, word_node, args) = (range?, word?, word_node?, args?);
    let positional = positional_args(args);
    let arg = |i: usize| positional.get(i).copied();
    let (method, url) = match word {
        "fetch" | "axios" => {
            let first = arg(0)?;
            let options = if first.kind() == "object" { Some(first) } else { arg(1) };
            let url = if first.kind() == "object" {
                keyword_value(first, "url", source)?
            } else {
                first
            };
            let method = options
                .filter(|o| o.kind() == "object")
                .and_then(|o| keyword_value(o, "method", source))
                .and_then(|v| verb(v, source))
                .unwrap_or("GET");
            (method, url)
        }
        "request" | "NewRequest" => (verb(arg(0)?, source)?, arg(1)?),
        "NewRequestWithContext" => (verb(arg(1)?, source)?, arg(2)?),
        w => (method_word(w)?, arg(0)?),
    };
    let raw = literal(url, source)?;
    let known = receiver.is_none_or(|r| CLIENT_MODULES.contains(&r));
    if !known && !is_absolute_http(raw) {
        return None;
    }
    let path = normalize_path(raw, true)?;
    Some(super::l2::call_at_name(format!("{method} {path}"), word_node, range))
}

fn is_absolute_http(raw: &str) -> bool {
    let lower = raw.get(..8).unwrap_or(raw).to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn text<'s>(node: Node, source: &'s [u8]) -> Option<&'s str> {
    node.utf8_text(source).ok()
}

/// The content of a string literal node, without prefixes (`f`, `r#`, `b`) and quotes; `None` for
/// any other node, so a URL built from a variable is never mistaken for a path.
fn literal<'s>(node: Node, source: &'s [u8]) -> Option<&'s str> {
    if !LITERAL_KINDS.contains(&node.kind()) {
        return None;
    }
    let raw = text(node, source)?;
    let body = raw.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let hashes = body.len() - body.trim_start_matches('#').len();
    let body = &body[hashes..];
    let quote = body.chars().next().filter(|c| matches!(c, '"' | '\'' | '`'))?;
    let triple: String = [quote; 3].iter().collect();
    let width = if body.len() >= 6 && body.starts_with(&triple) {
        3
    } else {
        1
    };
    let end = body.len().checked_sub(width + hashes)?;
    body.get(width..end.max(width))
}

/// The named arguments of an argument list, comments skipped.
fn positional_args(args: Node) -> Vec<Node> {
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .collect()
}

/// The value node of `key` in an argument list or object: a Python keyword argument, a JS
/// `key: value` pair or a Java `key = value` annotation element.
fn keyword_value<'t>(args: Node<'t>, key: &str, source: &[u8]) -> Option<Node<'t>> {
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        let (k, v) = match child.kind() {
            "keyword_argument" => (child.child_by_field_name("name"), child.child_by_field_name("value")),
            "pair" | "element_value_pair" => (child.child_by_field_name("key"), child.child_by_field_name("value")),
            _ => continue,
        };
        let (Some(k), Some(v)) = (k, v) else { continue };
        let name = literal(k, source).or_else(|| text(k, source));
        if name == Some(key) {
            return Some(v);
        }
    }
    None
}

/// The literal path in annotation arguments: the first positional string (or the first string
/// of a positional array), else the `value` or `path` element.
fn args_path<'s>(args: Node, source: &'s [u8]) -> Option<&'s str> {
    let first_literal = |n: Node| -> Option<&'s str> {
        literal(n, source).or_else(|| {
            let mut cursor = n.walk();
            n.named_children(&mut cursor).find_map(|c| literal(c, source))
        })
    };
    let positional = positional_args(args)
        .into_iter()
        .find(|n| !matches!(n.kind(), "element_value_pair" | "keyword_argument"));
    positional
        .and_then(first_literal)
        .or_else(|| keyword_value(args, "value", source).and_then(first_literal))
        .or_else(|| keyword_value(args, "path", source).and_then(first_literal))
}

/// The methods a route registered with `word` answers: Flask `route` reads `methods=[..]`
/// (default GET), FastAPI `api_route` too (default ANY), Spring `RequestMapping` reads
/// `method = ..` (default ANY), any other word is its own method.
fn route_methods(word: &str, args: Option<Node>, source: &[u8]) -> Vec<&'static str> {
    let (key, default) = match word {
        "route" => ("methods", "GET"),
        "api_route" => ("methods", ANY),
        "RequestMapping" => ("method", ANY),
        w => return method_word(w).into_iter().collect(),
    };
    let Some(value) = args.and_then(|a| keyword_value(a, key, source)) else {
        return vec![default];
    };
    let mut out: Vec<&'static str> = Vec::new();
    let mut push = |n: Node| {
        if let Some(m) = verb(n, source).filter(|m| !out.contains(m)) {
            out.push(m);
        }
    };
    if verb(value, source).is_some() {
        push(value);
    } else {
        let mut cursor = value.walk();
        for child in value.named_children(&mut cursor) {
            push(child);
        }
    }
    if out.is_empty() {
        out.push(default);
    }
    out
}

/// The method a verb node names: a string literal (`"POST"`) or a path ending in one
/// (`RequestMethod.POST`, `Method::GET`, `http.MethodGet`).
fn verb(node: Node, source: &[u8]) -> Option<&'static str> {
    let raw = literal(node, source).or_else(|| text(node, source))?;
    let last = raw.rsplit(['.', ':']).next().unwrap_or(raw);
    method_word(last)
}

/// The HTTP method a route or client word names: `get`, `GET`, `GetMapping`, `MethodGet`,
/// `PostForm`; `all` and `any` answer every method.
fn method_word(word: &str) -> Option<&'static str> {
    let w = word.strip_suffix("Mapping").unwrap_or(word);
    let w = w.strip_prefix("Method").filter(|r| !r.is_empty()).unwrap_or(w);
    Some(match w.to_ascii_lowercase().as_str() {
        "get" => "GET",
        "post" | "postform" => "POST",
        "put" => "PUT",
        "delete" => "DELETE",
        "patch" => "PATCH",
        "head" => "HEAD",
        "options" => "OPTIONS",
        "all" | "any" => ANY,
        _ => return None,
    })
}

fn handler_name(node: Node, source: &[u8]) -> Option<String> {
    if !HANDLER_KINDS.contains(&node.kind()) {
        return None;
    }
    text(node, source).filter(|t| !t.contains('\n')).map(str::to_owned)
}

/// The `(method, handler)` pairs of an axum method router: `get(h)`, `routing::post(h)`, and
/// chains such as `get(list).post(create).layer(..)`. Walks the chain down its receivers, so a
/// long chain costs no stack.
fn method_router(node: Node, source: &[u8], out: &mut Vec<(&'static str, Option<String>)>) {
    let mut found = Vec::new();
    let mut cur = Some(node);
    while let Some(call) = cur.filter(|n| n.kind() == "call_expression") {
        cur = None;
        let Some(function) = call.child_by_field_name("function") else {
            break;
        };
        let name = match function.kind() {
            "identifier" => Some(function),
            "scoped_identifier" => function.child_by_field_name("name"),
            "field_expression" => {
                cur = function.child_by_field_name("value");
                function.child_by_field_name("field")
            }
            _ => None,
        };
        let Some(method) = name.and_then(|n| text(n, source)).and_then(method_word) else {
            continue;
        };
        let handler = call
            .child_by_field_name("arguments")
            .and_then(|a| positional_args(a).first().copied())
            .and_then(|h| handler_name(h, source));
        found.push((method, handler));
    }
    out.extend(found.into_iter().rev());
}

/// Normalize a route or request path so both sides of an endpoint compare equal: every parameter
/// (`:id`, `{id}`, `{id:int}`, `<int:id>`, `${id}`, an f-string `{id}`) becomes `:param`, a
/// wildcard segment (`*filepath`) becomes `*`, and empty segments and a trailing `/` are dropped.
///
/// A client URL (`client`) also loses its query, fragment, scheme and host, and a leading
/// interpolation (a base URL in a variable); it needs a literal segment left over and an
/// `http(s)` scheme if it has one. `None` when nothing literal is left.
pub(crate) fn normalize_path(raw: &str, client: bool) -> Option<String> {
    let substituted = substitute_params(raw);
    let mut s = substituted.as_str();
    if client {
        s = s.split(['?', '#']).next().unwrap_or("");
        if let Some(i) = s.find("://") {
            if !matches!(s[..i].to_ascii_lowercase().as_str(), "http" | "https") {
                return None;
            }
            s = s[i + 3..].find('/').map_or("", |j| &s[i + 3 + j..]);
        } else if let Some(rest) = s.strip_prefix(SENTINEL) {
            // `${BASE}/users`: the base URL is not part of the path.
            s = rest.find('/').map_or("", |j| &rest[j..]);
        }
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut literal_segments = 0usize;
    for seg in s.split('/').filter(|seg| !seg.is_empty()) {
        out.push('/');
        if seg.starts_with('*') {
            out.push('*');
            continue;
        }
        let seg = normalize_segment(seg);
        if seg != PARAM {
            literal_segments += 1;
        }
        out.push_str(&seg);
    }
    if client && literal_segments == 0 && substituted.contains(SENTINEL) {
        return None;
    }
    if out.is_empty() {
        out.push('/');
    }
    Some(out)
}

/// Replace each `${..}`, `{..}` and `<..>` group (balanced braces) with [`SENTINEL`].
fn substitute_params(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        let close = match c {
            '$' if chars.peek() == Some(&'{') => {
                chars.next();
                '}'
            }
            '{' => '}',
            '<' => '>',
            _ => {
                out.push(c);
                continue;
            }
        };
        let open = if close == '}' { '{' } else { '<' };
        let mut depth = 1usize;
        for c in chars.by_ref() {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
        }
        out.push(SENTINEL);
    }
    out
}

/// One path segment with its parameters as `:param`: a `:name` (with an Express `(regex)` and a
/// `?`/`*`/`+` modifier) and every [`SENTINEL`].
fn normalize_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len() + 8);
    let mut rest = seg;
    if let Some(after) = rest.strip_prefix(':') {
        let name_len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        let mut tail = &after[name_len..];
        if tail.starts_with('(') {
            let mut depth = 0usize;
            let mut end = tail.len();
            for (i, c) in tail.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            tail = &tail[end..];
        }
        rest = tail.strip_prefix(['?', '*', '+']).unwrap_or(tail);
        out.push_str(PARAM);
    }
    for c in rest.chars() {
        if c == SENTINEL {
            out.push_str(PARAM);
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::normalize_path;
    use crate::lang::{QueryKind, try_get_query};

    #[test]
    fn route_and_client_sections_compile_for_each_language() {
        for lang in ["javascript", "typescript", "tsx", "python", "rust", "go", "java"] {
            let routes = try_get_query(lang, QueryKind::Routes).expect("routes compile");
            assert!(routes.is_some(), "{lang} has a routes section");
            let clients = try_get_query(lang, QueryKind::HttpCalls).expect("http_calls compile");
            assert_eq!(clients.is_some(), lang != "java", "{lang} http_calls section");
        }
    }

    #[test]
    fn every_parameter_syntax_normalizes_to_one_form() {
        for raw in [
            "/users/:id",
            "/users/{id}",
            "/users/{id:int}",
            "/users/<id>",
            "/users/<int:id>",
            "/users/${id}",
            "/users/:id(\\d+)",
            "/users/:id?",
            "users/{user_id}/",
            "//users///{id}",
        ] {
            assert_eq!(normalize_path(raw, false).as_deref(), Some("/users/:param"), "{raw}");
        }
        assert_eq!(normalize_path("", false).as_deref(), Some("/"));
        assert_eq!(normalize_path("/static/*filepath", false).as_deref(), Some("/static/*"));
        assert_eq!(
            normalize_path("/f/:name.json", false).as_deref(),
            Some("/f/:param.json")
        );
    }

    #[test]
    fn client_urls_lose_scheme_host_query_and_base() {
        for raw in [
            "https://api.example.com/users/${id}?full=1",
            "http://localhost:8080/users/{id}#top",
            "${BASE}/users/${id}",
            "{BASE_URL}/users/{uid}",
            "https://${host}/users/:id",
            "/users/${id}",
        ] {
            assert_eq!(normalize_path(raw, true).as_deref(), Some("/users/:param"), "{raw}");
        }
        assert_eq!(normalize_path("/users/42", true).as_deref(), Some("/users/42"));
        assert_eq!(normalize_path("https://example.com", true).as_deref(), Some("/"));
        assert_eq!(normalize_path("${BASE}${path}", true), None);
        assert_eq!(normalize_path("${url}", true), None);
        assert_eq!(normalize_path("ws://host/socket", true), None);
    }
}
