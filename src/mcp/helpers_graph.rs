//! Body of the `call_graph` MCP tool.
//!
//! BFS-walks the call graph from a root name in either direction:
//!
//! - `direction = "callers"` — for each frontier symbol F, prefix-scan
//!   `calls_by_callee` on F's name → resolve each call site's containing function
//!   via the in-RAM L1 outline → push containing functions as the next frontier.
//! - `direction = "callees"` — for each frontier symbol F, find F's L1 symbol(s),
//!   prefix-scan `calls_by_path` for F's file, filter to calls inside F's byte
//!   range → push unique callee names as the next frontier.
//!
//! Cycle detection is name-keyed: a visited set of strings. Recursive functions
//! land at the root with one self-edge.

use std::collections::VecDeque;
use std::ops::Bound;

use ahash::{AHashMap, AHashSet};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::MapCache;
use super::cursor::prefix_upper_bound;
use super::helpers::{json_result, kind_to_str};
use super::helpers_calls::for_each_call_in_file;
use super::types_graph::{CallGraphNode, CallGraphParams, CallGraphResponse, CallGraphSite};
use crate::extract::{FileMapL1, Symbol, SymbolKind};
use crate::path::RelPath;

const MAX_DEPTH_CEILING: u32 = 6;
const MAX_NODES_CEILING: u32 = 500;
const DEFAULT_MAX_DEPTH: u32 = 3;
const DEFAULT_MAX_NODES: u32 = 100;

/// Function-like symbol kinds that can act as call-graph nodes. A call site whose
/// enclosing symbol is not one of these (e.g. a top-level expression in a Python
/// module body) is treated as file-scope and dropped — there's no parent function
/// to attribute the call to.
pub(super) fn is_function_like(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor | SymbolKind::Getter | SymbolKind::Setter
    )
}

/// Entry point — wraps the BFS body, packs the result into a `CallToolResult`.
pub(super) fn run_call_graph(
    idx: Option<&crate::index::IndexDb>,
    params: CallGraphParams,
    cache: &MapCache,
) -> Result<CallToolResult, McpError> {
    let direction = params.direction.as_str();
    let direction_owned = match direction {
        "callers" | "callees" => direction.to_string(),
        other => {
            return Err(McpError::invalid_params(
                format!("direction must be \"callers\" or \"callees\", got {other:?}"),
                None,
            ));
        }
    };
    let max_depth = params.max_depth.unwrap_or(DEFAULT_MAX_DEPTH).min(MAX_DEPTH_CEILING);
    let max_nodes = params.max_nodes.unwrap_or(DEFAULT_MAX_NODES).min(MAX_NODES_CEILING) as usize;

    // A read-only session (no Fjall) routes through the in-RAM call index built
    // from the shared blobs — see `collect_callers` / `collect_callees_for_name`.
    let outcome = if direction == "callers" {
        bfs_callers(idx, cache, &params.name, params.path.as_ref(), max_depth, max_nodes)?
    } else {
        bfs_callees(idx, cache, &params.name, params.path.as_ref(), max_depth, max_nodes)?
    };

    json_result(&CallGraphResponse {
        root: params.name,
        direction: direction_owned,
        nodes: outcome.nodes,
        truncated: outcome.truncated,
        truncation_reason: outcome.truncation_reason,
    })
}

struct BfsOutcome {
    nodes: Vec<CallGraphNode>,
    truncated: bool,
    truncation_reason: Option<&'static str>,
}

/// Build the root node: every definition site of `name` (filtered to function-like
/// kinds, optionally restricted to `path_filter`).
fn build_root(name: &str, cache: &MapCache, path_filter: Option<&RelPath>) -> CallGraphNode {
    let mut sites: Vec<CallGraphSite> = Vec::new();
    let iter: Box<dyn Iterator<Item = (&RelPath, &FileMapL1)>> = match path_filter {
        Some(p) => match cache.by_path.get(p) {
            Some(l1) => Box::new(std::iter::once((p, l1))),
            None => Box::new(std::iter::empty()),
        },
        None => Box::new(cache.by_path.iter()),
    };
    for (path, l1) in iter {
        for sym in &l1.symbols {
            if sym.name == name && is_function_like(sym.kind) {
                sites.push(CallGraphSite {
                    path: path.clone(),
                    kind: kind_to_str(sym.kind).to_string(),
                    start_row: sym.start_row,
                    start_col: sym.start_col,
                });
            }
        }
    }
    CallGraphNode {
        name: name.to_string(),
        depth: 0,
        edges_to: Vec::new(),
        sites,
    }
}

/// Locate the function-like symbol that contains `start_byte` in `l1`. Picks the
/// *tightest* match (smallest byte range that still covers `start_byte`) so nested
/// function definitions attribute correctly. Returns `None` when no function-like
/// symbol wraps the call site (file-scope call).
fn containing_function(l1: &FileMapL1, start_byte: u32) -> Option<&Symbol> {
    let mut best: Option<&Symbol> = None;
    for sym in &l1.symbols {
        if !is_function_like(sym.kind) {
            continue;
        }
        if sym.start_byte <= start_byte && start_byte < sym.end_byte {
            let span = sym.end_byte.saturating_sub(sym.start_byte);
            let best_span = best
                .map(|s| s.end_byte.saturating_sub(s.start_byte))
                .unwrap_or(u32::MAX);
            if span < best_span {
                best = Some(sym);
            }
        }
    }
    best
}

/// BFS upward: who calls into `name`?
fn bfs_callers(
    idx: Option<&crate::index::IndexDb>,
    cache: &MapCache,
    root_name: &str,
    path_filter: Option<&RelPath>,
    max_depth: u32,
    max_nodes: usize,
) -> Result<BfsOutcome, McpError> {
    let mut nodes: Vec<CallGraphNode> = vec![build_root(root_name, cache, path_filter)];
    let mut index_of: AHashMap<String, u32> = AHashMap::new();
    index_of.insert(root_name.to_string(), 0);

    let mut frontier: VecDeque<(String, u32)> = VecDeque::new();
    frontier.push_back((root_name.to_string(), 0));

    let scan_cap = max_nodes.saturating_mul(8).max(2_000);
    let mut truncated = false;
    let mut truncation_reason: Option<&'static str> = None;
    let mut hit_scan_cap = false;
    let mut depth_gated = false;

    while let Some((current_name, depth)) = frontier.pop_front() {
        if depth >= max_depth {
            // We won't expand at the max depth — record that there *may* be more
            // beyond. Honest "may" — when this frontier entry has no actual callers,
            // marking truncated is a false positive, but we'd have to do the full
            // scan to know, defeating the purpose of the cap.
            depth_gated = true;
            continue;
        }
        // Gather unique parent (containing function) names for this frontier entry.
        let parents = match collect_callers(idx, cache, &current_name, scan_cap) {
            Ok(p) => p,
            Err(CallerScanError::ScanCap) => {
                hit_scan_cap = true;
                AHashMap::new()
            }
            Err(CallerScanError::Other(e)) => return Err(e),
        };

        let current_idx = *index_of.get(&current_name).expect("frontier entry must be indexed");

        for (parent_name, parent_sites) in parents {
            // Self-recursion: add a self-edge and stop expanding.
            if parent_name == current_name {
                if !nodes[current_idx as usize].edges_to.contains(&current_idx) {
                    nodes[current_idx as usize].edges_to.push(current_idx);
                }
                continue;
            }
            let already = index_of.get(&parent_name).copied();
            let parent_idx = match already {
                Some(i) => i,
                None => {
                    if nodes.len() >= max_nodes {
                        truncated = true;
                        truncation_reason = Some("max_nodes");
                        break;
                    }
                    let new_idx = nodes.len() as u32;
                    nodes.push(CallGraphNode {
                        name: parent_name.clone(),
                        depth: depth + 1,
                        edges_to: Vec::new(),
                        sites: parent_sites,
                    });
                    index_of.insert(parent_name.clone(), new_idx);
                    frontier.push_back((parent_name, depth + 1));
                    new_idx
                }
            };
            // Edge: parent (callers direction) points to current (parent → current).
            if !nodes[parent_idx as usize].edges_to.contains(&current_idx) {
                nodes[parent_idx as usize].edges_to.push(current_idx);
            }
        }
        if truncation_reason == Some("max_nodes") {
            break;
        }
    }

    // Order of precedence: max_nodes already short-circuited above. Then scan_cap
    // (hard work-bound), then depth_gated (we stopped expanding at the boundary).
    if truncation_reason.is_none() && hit_scan_cap {
        truncated = true;
        truncation_reason = Some("scan_cap");
    }
    if truncation_reason.is_none() && depth_gated {
        truncated = true;
        truncation_reason = Some("max_depth");
    }

    Ok(BfsOutcome {
        nodes,
        truncated,
        truncation_reason,
    })
}

enum CallerScanError {
    ScanCap,
    Other(McpError),
}

/// For one frontier name, return `{ parent_name → [definition sites] }` of every
/// containing function that calls `name`.
fn collect_callers(
    idx: Option<&crate::index::IndexDb>,
    cache: &MapCache,
    name: &str,
    scan_cap: usize,
) -> Result<AHashMap<String, Vec<CallGraphSite>>, CallerScanError> {
    let mut parents: AHashMap<String, Vec<CallGraphSite>> = AHashMap::new();
    // Dedupe by (path, start_row, start_col) of the *parent symbol* — a function
    // definition has a unique (file, start position) triple, so this key is sufficient
    // to prevent adding the same parent site twice when one function calls `name` many
    // times. The name dimension is not needed: two distinct functions cannot share the
    // same (file, row, col).
    let mut seen_sites: AHashSet<(RelPath, u32, u32)> = AHashSet::new();
    let mut scanned: usize = 0;

    // Per-call-site closure shared by both backends: resolve the containing function
    // and record its definition site once.
    let mut record = |rel: RelPath, start_byte: u32| {
        let Some(l1) = cache.by_path.get(&rel) else {
            return;
        };
        let Some(parent_sym) = containing_function(l1, start_byte) else {
            return;
        };
        let site = CallGraphSite {
            path: rel.clone(),
            kind: kind_to_str(parent_sym.kind).to_string(),
            start_row: parent_sym.start_row,
            start_col: parent_sym.start_col,
        };
        if seen_sites.insert((rel, site.start_row, site.start_col)) {
            parents.entry(parent_sym.name.clone()).or_default().push(site);
        }
    };

    match idx {
        Some(idx) => {
            let prefix = crate::index::keys::calls_by_callee_prefix(name);
            let upper_bound: Bound<Vec<u8>> = match prefix_upper_bound(&prefix) {
                Some(b) => Bound::Excluded(b),
                None => Bound::Unbounded,
            };
            let lower = Bound::Included(prefix);
            for guard in idx.calls_by_callee.range::<Vec<u8>, _>((lower, upper_bound)) {
                scanned += 1;
                if scanned > scan_cap {
                    return Err(CallerScanError::ScanCap);
                }
                let (k, _) = guard
                    .into_inner()
                    .map_err(|e| CallerScanError::Other(McpError::internal_error(format!("index iter: {e}"), None)))?;
                let Some((callee, rel, start_byte)) = crate::index::keys::parse_call_by_callee(&k) else {
                    continue;
                };
                // Defensive exact-name guard (the length-prefixed key already ensures it).
                if callee != name {
                    continue;
                }
                record(rel, start_byte);
            }
        }
        None => {
            let Some(calls) = cache.calls.as_ref() else {
                return Ok(parents);
            };
            for (rel, start_byte) in calls.callers_of(name) {
                scanned += 1;
                if scanned > scan_cap {
                    return Err(CallerScanError::ScanCap);
                }
                record(rel.clone(), start_byte);
            }
        }
    }
    Ok(parents)
}

/// BFS downward: what does `name` itself call?
fn bfs_callees(
    idx: Option<&crate::index::IndexDb>,
    cache: &MapCache,
    root_name: &str,
    path_filter: Option<&RelPath>,
    max_depth: u32,
    max_nodes: usize,
) -> Result<BfsOutcome, McpError> {
    let mut nodes: Vec<CallGraphNode> = vec![build_root(root_name, cache, path_filter)];
    let mut index_of: AHashMap<String, u32> = AHashMap::new();
    index_of.insert(root_name.to_string(), 0);

    let mut frontier: VecDeque<(String, u32)> = VecDeque::new();
    frontier.push_back((root_name.to_string(), 0));

    let scan_cap = max_nodes.saturating_mul(8).max(2_000);
    let mut truncated = false;
    let mut truncation_reason: Option<&'static str> = None;
    let mut hit_scan_cap = false;
    let mut depth_gated = false;

    // Precompute function-like symbol sites per name once, O(n_all_symbols). The original
    // code scanned all of cache.by_path for every newly-discovered callee (O(max_nodes ×
    // n_all_symbols)). Iterates cache.by_path in the same BTreeMap ascending order so the
    // sites Vec per name is byte-identical to what the old inline scan produced.
    let mut name_to_sites: AHashMap<String, Vec<CallGraphSite>> = AHashMap::new();
    for (path, l1) in &cache.by_path {
        for sym in &l1.symbols {
            if is_function_like(sym.kind) {
                name_to_sites.entry(sym.name.clone()).or_default().push(CallGraphSite {
                    path: path.clone(),
                    kind: kind_to_str(sym.kind).to_string(),
                    start_row: sym.start_row,
                    start_col: sym.start_col,
                });
            }
        }
    }

    while let Some((current_name, depth)) = frontier.pop_front() {
        if depth >= max_depth {
            depth_gated = true;
            continue;
        }
        // Collect the callees of `current_name` by walking every definition site
        // (or just the one at path_filter, when frontier == root).
        let callees = match collect_callees_for_name(
            idx,
            cache,
            &current_name,
            if depth == 0 { path_filter } else { None },
            scan_cap,
        ) {
            Ok(c) => c,
            Err(CallerScanError::ScanCap) => {
                hit_scan_cap = true;
                AHashSet::new()
            }
            Err(CallerScanError::Other(e)) => return Err(e),
        };

        let current_idx = *index_of.get(&current_name).expect("frontier entry must be indexed");

        for callee in callees {
            if callee == current_name {
                if !nodes[current_idx as usize].edges_to.contains(&current_idx) {
                    nodes[current_idx as usize].edges_to.push(current_idx);
                }
                continue;
            }
            let already = index_of.get(&callee).copied();
            let child_idx = match already {
                Some(i) => i,
                None => {
                    if nodes.len() >= max_nodes {
                        truncated = true;
                        truncation_reason = Some("max_nodes");
                        break;
                    }
                    let new_idx = nodes.len() as u32;
                    // Look up definition sites from the precomputed map (O(1) per callee)
                    // instead of re-scanning all symbols on every newly-discovered callee.
                    // May be empty for external library functions not in the index.
                    let sites = name_to_sites.get(callee.as_str()).cloned().unwrap_or_default();
                    nodes.push(CallGraphNode {
                        name: callee.clone(),
                        depth: depth + 1,
                        edges_to: Vec::new(),
                        sites,
                    });
                    index_of.insert(callee.clone(), new_idx);
                    frontier.push_back((callee, depth + 1));
                    new_idx
                }
            };
            // Edge: current (callees direction) points to child (current → child).
            if !nodes[current_idx as usize].edges_to.contains(&child_idx) {
                nodes[current_idx as usize].edges_to.push(child_idx);
            }
        }
        if truncation_reason == Some("max_nodes") {
            break;
        }
    }

    if truncation_reason.is_none() && hit_scan_cap {
        truncated = true;
        truncation_reason = Some("scan_cap");
    }
    if truncation_reason.is_none() && depth_gated {
        truncated = true;
        truncation_reason = Some("max_depth");
    }

    Ok(BfsOutcome {
        nodes,
        truncated,
        truncation_reason,
    })
}

/// Collect unique callee names invoked from inside every definition site of `name`
/// (optionally restricted to one path).
fn collect_callees_for_name(
    idx: Option<&crate::index::IndexDb>,
    cache: &MapCache,
    name: &str,
    path_filter: Option<&RelPath>,
    scan_cap: usize,
) -> Result<AHashSet<String>, CallerScanError> {
    let mut callees: AHashSet<String> = AHashSet::new();
    let mut scanned: usize = 0;

    let iter: Box<dyn Iterator<Item = (&RelPath, &FileMapL1)>> = match path_filter {
        Some(p) => match cache.by_path.get(p) {
            Some(l1) => Box::new(std::iter::once((p, l1))),
            None => Box::new(std::iter::empty()),
        },
        None => Box::new(cache.by_path.iter()),
    };

    for (path, l1) in iter {
        // Collect every function-like symbol in this file whose name matches.
        let matching: Vec<&Symbol> = l1
            .symbols
            .iter()
            .filter(|s| s.name == name && is_function_like(s.kind))
            .collect();
        if matching.is_empty() {
            continue;
        }
        // Delegate the dual-backend scan (Fjall prefix vs in-RAM call index) to the shared
        // helper. Same cap semantics: `scanned` increments for every call site, `cap_hit`
        // stops the current file, checked after each file to short-circuit further iteration.
        let mut cap_hit = false;
        for_each_call_in_file(idx, cache, path, |callee, start_byte| {
            scanned += 1;
            if scanned > scan_cap {
                cap_hit = true;
                return false;
            }
            if matching
                .iter()
                .any(|s| s.start_byte <= start_byte && start_byte < s.end_byte)
            {
                callees.insert(callee.to_string());
            }
            true
        })
        .map_err(CallerScanError::Other)?;
        if cap_hit {
            return Err(CallerScanError::ScanCap);
        }
    }
    Ok(callees)
}
