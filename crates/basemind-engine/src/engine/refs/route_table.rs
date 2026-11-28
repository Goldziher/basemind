//! The route table `code_refs` links HTTP calls against (ADR-0048): every indexed route, built once
//! per map snapshot (`crate::engine::MapCache::routes`) and bucketed so a request is scored only
//! against the routes that can match it.
//!
//! The buckets follow from how `super::routes::score` aligns two paths. A route without a tail
//! wildcard matches segment for segment (same length) or by a suffix that holds a literal segment,
//! so both sides line up at their ends and the route's last literal segment meets the request
//! segment at the same distance from the end: those routes are keyed by that literal and distance.
//! A route with no literal segment matches only exactly, so it is keyed by its length. A route with
//! a tail wildcard lines up at its start or anywhere inside a longer request; those are few (a
//! catch-all, a static mount) and are scored against every request.

use ahash::AHashMap;

use super::routes::{Endpoint, is_literal};
use crate::engine::MapCache;
use crate::extract::SymbolKind;
use crate::path::RelPath;

/// Every route name holds this between its method and its path.
const METHOD_PATH_SEPARATOR: &str = " /";

/// One indexed route.
#[derive(Debug, Clone)]
pub struct RouteDef {
    pub(crate) path: RelPath,
    pub(crate) name: String,
    pub(crate) endpoint: Endpoint,
    /// The handler as the registration writes it (`show_user`, `users.show`, `handlers::show`).
    pub(crate) handler: Option<String>,
    /// 1-based line of the route registration.
    pub(crate) line: u32,
    /// 0-based byte column.
    pub(crate) column: u32,
    /// Byte offset of the registration: the position a route row sorts by among call sites.
    pub(crate) start_byte: u32,
}

/// Every route the index holds, bucketed for lookup by request and by handler.
#[derive(Debug, Default)]
pub struct Routes {
    defs: Vec<RouteDef>,
    /// Routes without a tail wildcard, by their last literal segment: `(segments from the end,
    /// route)`.
    by_literal: AHashMap<String, Vec<(u32, u32)>>,
    /// Routes without a tail wildcard or a literal segment, by segment count.
    by_length: AHashMap<usize, Vec<u32>>,
    /// Routes with a tail wildcard.
    wildcard: Vec<u32>,
    /// Routes by the last component of their handler (`show` for `users.show`).
    by_handler: AHashMap<String, Vec<u32>>,
}

impl Routes {
    /// Every route `cache` holds, read from its term index; positions and handlers come from the
    /// outlines of the files that hold routes. Not capped: [`MapCache::routes`] builds it once per
    /// snapshot, and a route is a few hundred bytes.
    pub(crate) fn of(cache: &MapCache) -> Self {
        let finder = memchr::memmem::Finder::new(METHOD_PATH_SEPARATOR.as_bytes());
        let mut defs = Vec::new();
        for (path, terms) in cache.terms().iter() {
            let indices: Vec<usize> = terms.matching_symbols(&finder, Some(SymbolKind::Route)).collect();
            if indices.is_empty() {
                continue;
            }
            let Some(l1) = cache.get(path) else {
                continue;
            };
            for symbol in indices.into_iter().filter_map(|idx| l1.symbols.get(idx)) {
                let Some(endpoint) = Endpoint::parse(&symbol.name) else {
                    continue;
                };
                defs.push(RouteDef {
                    path: path.clone(),
                    name: symbol.name.clone(),
                    endpoint,
                    handler: symbol.signature.clone(),
                    line: symbol.start_row + 1,
                    column: symbol.start_col,
                    start_byte: symbol.start_byte,
                });
            }
        }
        Self::from_defs(defs)
    }

    /// The table over `defs`, bucketed.
    pub(crate) fn from_defs(defs: Vec<RouteDef>) -> Self {
        let mut table = Self::default();
        for (at, def) in defs.iter().enumerate() {
            let at = u32::try_from(at).unwrap_or(u32::MAX);
            let segments = &def.endpoint.segments;
            if def.endpoint.wildcard_tail() {
                table.wildcard.push(at);
            } else if let Some((from_end, literal)) = segments.iter().rev().enumerate().find(|(_, seg)| is_literal(seg))
            {
                table
                    .by_literal
                    .entry(literal.clone())
                    .or_default()
                    .push((u32::try_from(from_end).unwrap_or(u32::MAX), at));
            } else {
                table.by_length.entry(segments.len()).or_default().push(at);
            }
            if let Some(handler) = def.handler.as_deref().map(handler_key).filter(|key| !key.is_empty()) {
                table.by_handler.entry(handler.to_owned()).or_default().push(at);
            }
        }
        table.defs = defs;
        table
    }

    #[cfg(test)]
    pub(crate) fn from_names(names: &[(&str, &str)]) -> Self {
        Self::from_defs(
            names
                .iter()
                .enumerate()
                .map(|(line, (path, name))| RouteDef {
                    path: RelPath::from(*path),
                    name: (*name).to_string(),
                    endpoint: Endpoint::parse(name).expect("a route name"),
                    handler: None,
                    line: line as u32 + 1,
                    column: 0,
                    start_byte: 0,
                })
                .collect(),
        )
    }

    /// Routes held.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.defs.len()
    }

    /// The routes that can match the request `call`: a superset of those [`super::routes::score`]
    /// matches, each once.
    pub(crate) fn candidates<'a, 'c>(&'a self, call: &'c Endpoint) -> impl Iterator<Item = &'a RouteDef> + use<'a, 'c> {
        let by_literal = call
            .segments
            .iter()
            .rev()
            .enumerate()
            .filter(|(_, seg)| is_literal(seg))
            .flat_map(move |(from_end, seg)| {
                self.by_literal
                    .get(seg.as_str())
                    .into_iter()
                    .flatten()
                    .filter(move |(at_from_end, _)| *at_from_end as usize == from_end)
                    .map(|(_, at)| *at)
            });
        let by_length = self.by_length.get(&call.segments.len()).into_iter().flatten().copied();
        by_literal
            .chain(by_length)
            .chain(self.wildcard.iter().copied())
            .filter_map(|at| self.defs.get(at as usize))
    }

    /// Whether a route of exactly `endpoint` is indexed.
    pub(crate) fn holds(&self, endpoint: &Endpoint) -> bool {
        self.candidates(endpoint).any(|route| route.endpoint == *endpoint)
    }

    /// The routes whose handler's last component is `name`.
    pub(crate) fn naming<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a RouteDef> + 'a {
        self.by_handler
            .get(name)
            .into_iter()
            .flatten()
            .filter_map(|at| self.defs.get(*at as usize))
    }
}

/// The name a handler expression calls: its last component (`show` for `users.show`,
/// `handlers::show` and `h.show`).
fn handler_key(handler: &str) -> &str {
    handler.rsplit(['.', ':']).next().unwrap_or(handler).trim()
}

#[cfg(test)]
#[path = "route_table_tests.rs"]
mod tests;
