//! The shared, typed, provenance-tagged code-graph model (ADR-0001 / ADR-0002).
//!
//! `codegraph` builds one in-memory, multi-edge graph over the relationships basemind
//! already indexes — calls, imports, inheritance, resolved references, and containment —
//! with every edge tagged by [`Provenance`] (how strongly we believe the target binding)
//! and a numeric confidence. It is **built on demand and discarded**: a read-side
//! abstraction with no persisted state and no schema bump. `architecture_map` is its first
//! consumer; traversal, communities, and rendering (later ADRs) will read the same model.
//!
//! Node identity is a resolved symbol location ([`NodeKey::Symbol`]), a whole-file node
//! ([`NodeKey::File`], the source of imports/containment), or a **virtual name node**
//! ([`NodeKey::Name`]) when a target name does not resolve to a definition — so the graph
//! stays well-defined for exactly the inferred/ambiguous edges provenance tags.
//!
//! Provenance is **derived on read** and degrades *down*: a binding whose proof is
//! unreachable (no open index, cross-file in a degraded mode) is reported INFERRED, never
//! falsely EXTRACTED. Import and inheritance edges are name-resolved by construction, so
//! they are INFERRED (one candidate) or AMBIGUOUS (several) — never EXTRACTED to a node.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, PoisonError};

use ahash::{AHashMap, AHashSet};
use lru::LruCache;
use rmcp::ErrorData as McpError;

use super::MapCache;
use super::helpers_calls::for_each_call_in_file;
use super::helpers_graph::is_function_like;
use crate::index::IndexDb;
use crate::path::RelPath;

/// The kind of relationship an edge represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum EdgeKind {
    /// A call site whose callee name resolves to a function-like definition.
    Calls,
    /// A file importing a module/symbol.
    Imports,
    /// An `impl`/subclass/`implements`/`extends` relationship.
    Inherits,
    /// A file→symbol containment (nesting) edge — structural.
    Contains,
}

impl EdgeKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Calls => "calls",
            EdgeKind::Imports => "imports",
            EdgeKind::Inherits => "inherits",
            EdgeKind::Contains => "contains",
        }
    }
}

/// How strongly the edge's *target node* is believed. See ADR-0002.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Provenance {
    /// Proven — a resolved use→definition binding, or a structurally explicit edge.
    Extracted,
    /// Name-level — the target matched by name but resolution did not prove it.
    Inferred,
    /// Heuristic or one-name-to-many — a name resolving to several candidate definitions.
    Ambiguous,
}

impl Provenance {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Provenance::Extracted => "extracted",
            Provenance::Inferred => "inferred",
            Provenance::Ambiguous => "ambiguous",
        }
    }

    /// Numeric confidence on the fixed ladder (ADR-0002): a default traversal edge weight
    /// and a renderer styling signal.
    pub(crate) fn confidence(self) -> f32 {
        match self {
            Provenance::Extracted => 1.0,
            Provenance::Inferred => 0.5,
            Provenance::Ambiguous => 0.2,
        }
    }

    /// Aggregation rank — higher is stronger. Used to fold many underlying edges into one
    /// coarser edge by keeping the strongest provenance ("is this relationship grounded at
    /// all"). Never manufactures EXTRACTED that no underlying edge carried.
    pub(crate) fn rank(self) -> u8 {
        match self {
            Provenance::Extracted => 2,
            Provenance::Inferred => 1,
            Provenance::Ambiguous => 0,
        }
    }
}

/// A node in the code graph. `Symbol` and `File` are resolved locations; `Name` is a
/// virtual node for a target that did not resolve to a definition.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum NodeKey {
    Symbol { path: RelPath, start_byte: u32 },
    File { path: RelPath },
    Name(String),
}

impl NodeKey {
    /// The owning file path, when the node has one (`Symbol`/`File`). `None` for a virtual
    /// name node. Consumers that aggregate to file/module granularity drop `None` endpoints.
    pub(crate) fn file(&self) -> Option<&RelPath> {
        match self {
            NodeKey::Symbol { path, .. } | NodeKey::File { path } => Some(path),
            NodeKey::Name(_) => None,
        }
    }
}

/// One typed, provenance-tagged edge.
#[derive(Debug, Clone)]
pub(crate) struct CodeEdge {
    pub(crate) from: NodeKey,
    pub(crate) to: NodeKey,
    pub(crate) kind: EdgeKind,
    pub(crate) provenance: Provenance,
    /// Aggregate multiplicity (e.g. call-site count); 1 for structural edges.
    pub(crate) weight: u32,
}

/// Which edge lanes to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct EdgeKindSet {
    pub(crate) calls: bool,
    pub(crate) imports: bool,
    pub(crate) inherits: bool,
    pub(crate) contains: bool,
}

impl EdgeKindSet {
    /// Every lane — the full model (tests today; traversal/rendering in later ADRs).
    #[cfg(test)]
    pub(crate) fn all() -> Self {
        Self {
            calls: true,
            imports: true,
            inherits: true,
            contains: true,
        }
    }

    fn none() -> Self {
        Self {
            calls: false,
            imports: false,
            inherits: false,
            contains: false,
        }
    }

    /// Whether the given edge kind is one of the selected lanes. Traversal (ADR-0003) uses
    /// this to filter edges to the lanes a query asked for.
    pub(crate) fn contains_kind(&self, kind: EdgeKind) -> bool {
        match kind {
            EdgeKind::Calls => self.calls,
            EdgeKind::Imports => self.imports,
            EdgeKind::Inherits => self.inherits,
            EdgeKind::Contains => self.contains,
        }
    }

    /// Map `architecture_map`'s `edges` param to lanes. `contains` is noise at file/module
    /// granularity (self-loops), so `"all"` here means calls+imports+inherits.
    pub(crate) fn from_edges_param(s: &str) -> Self {
        match s {
            "imports" => Self {
                imports: true,
                ..Self::none()
            },
            "inherits" => Self {
                inherits: true,
                ..Self::none()
            },
            "both" => Self {
                calls: true,
                imports: true,
                ..Self::none()
            },
            "all" => Self {
                calls: true,
                imports: true,
                inherits: true,
                ..Self::none()
            },
            // "calls" and any unrecognized value keep the historical default: calls only.
            _ => Self {
                calls: true,
                ..Self::none()
            },
        }
    }
}

/// Default hard cap on call sites scanned when building the graph — shared by every on-demand
/// consumer (`architecture_map`, the traversal tools, community detection) so the bound is bumped
/// in one place. Bounds work on huge repos; a hub root cannot trigger unbounded work.
pub(crate) const CODEGRAPH_SCAN_CAP: usize = 4_000_000;

/// Inputs to a graph build.
pub(crate) struct BuildOpts {
    pub(crate) kinds: EdgeKindSet,
    /// Repo-relative path prefix to scope the build; `None` = whole repo.
    pub(crate) focus: Option<String>,
    /// Hard cap on call sites scanned (bounds work on huge repos).
    pub(crate) scan_cap: usize,
}

/// The built graph. Edges are sorted for deterministic output.
pub(crate) struct CodeGraph {
    pub(crate) edges: Vec<CodeEdge>,
    /// True when the call scan hit `scan_cap` and the graph is over a partial set.
    pub(crate) truncated: bool,
}

/// Cache key for a built graph. `fingerprint` is the content fingerprint of the [`MapCache`] the
/// graph was built from — read from that same cache, so a key can never name a graph built from a
/// different snapshot than it claims. The lanes and `focus` prefix select which relationships are
/// built; `idx_present` records whether a live index proved call edges (`Some`/`None` changes the
/// provenance tier). `min_confidence` is deliberately absent — it is a per-call post-build filter.
pub(crate) type GraphKey = (u64, EdgeKindSet, Option<String>, bool);

/// Capacity of the graph memo — distinct `(fingerprint, lanes, focus, idx-mode)` graphs held at once.
/// `focus` is caller-supplied so the key space is unbounded in principle; an LRU bounds RAM while
/// letting several fingerprints and lane sets coexist (e.g. across a rescan window) rather than
/// thrashing a single slot. Small: the working set is a handful of lane/focus combos per snapshot.
const GRAPH_MEMO_CAP: usize = 16;

/// The graph memo: an LRU of built [`CodeGraph`]s shared by every graph tool (ADR-0001..0005). Each
/// entry is keyed by the content fingerprint of the cache it was built from, so a superseded graph is
/// simply a key no current cache matches — it ages out by LRU rather than ever being served. Repeat
/// calls against one snapshot collapse to an `Arc` clone.
pub(crate) type GraphMemo = LruCache<GraphKey, Arc<CodeGraph>>;

/// An empty graph memo at the shared capacity.
pub(crate) fn new_graph_memo() -> GraphMemo {
    LruCache::new(NonZeroUsize::new(GRAPH_MEMO_CAP).expect("GRAPH_MEMO_CAP > 0"))
}

/// Build the graph for `opts`, served from `memo` when a graph for the same
/// `(fingerprint, lanes, focus, idx-mode)` was already built. The fingerprint is read from `cache`
/// itself, so a hit is always a graph built from the exact snapshot the caller holds — no separate
/// generation counter to fall out of step with it. On a miss the build runs OUTSIDE the lock: it can
/// take tens of ms on a large repo, so holding the mutex across it would stall every other graph
/// tool, and a rare concurrent double-build is bounded and harmless (both produce the same
/// deterministic graph). A poisoned lock is recovered — a lost memo entry only costs one rebuild.
pub(crate) fn build_memoized(
    memo: &Mutex<GraphMemo>,
    idx: Option<&IndexDb>,
    cache: &MapCache,
    opts: &BuildOpts,
) -> Result<Arc<CodeGraph>, McpError> {
    // The key omits scan_cap; every consumer must pass the shared cap or a hit could return a graph
    // truncated under a different bound.
    debug_assert_eq!(opts.scan_cap, CODEGRAPH_SCAN_CAP);
    let key: GraphKey = (cache.fingerprint, opts.kinds, opts.focus.clone(), idx.is_some());
    if let Some(hit) = memo.lock().unwrap_or_else(PoisonError::into_inner).get(&key).cloned() {
        return Ok(hit);
    }
    let graph = Arc::new(build(idx, cache, opts)?);
    memo.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .put(key, Arc::clone(&graph));
    Ok(graph)
}

/// The leaf identifier of a module path — the last non-empty component after splitting on
/// `.`, `/`, or `:`. `crate::mod_a::Widget` → `Widget`, `a/b/c` → `c`, `foo.bar` → `bar`.
fn module_leaf(module: &str) -> &str {
    module.rsplit(['.', '/', ':']).find(|s| !s.is_empty()).unwrap_or(module)
}

/// The trailing identifier in an import's raw text — the fallback when the grammar didn't
/// capture a clean module (e.g. Rust `use crate::mod_a::Widget;` → `Widget`). Mirrors the
/// raw-text matching `dependents_of` already relies on.
fn trailing_identifier(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut end = bytes.len();
    while end > 0 && !is_ident(bytes[end - 1]) {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    let mut start = end;
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    Some(&raw[start..end])
}

/// The best-effort identifier an import resolves against: the module leaf when the grammar
/// captured a module, else the trailing identifier of the raw statement.
fn import_leaf(imp: &crate::extract::Import) -> Option<&str> {
    if let Some(m) = imp.module.as_deref() {
        let leaf = module_leaf(m);
        if !leaf.is_empty() {
            return Some(leaf);
        }
    }
    trailing_identifier(&imp.raw)
}

/// Accumulator key: an edge is unique by `(from, to, kind)`; weight sums and provenance
/// folds to the strongest tier across duplicates.
type EdgeKey = (NodeKey, NodeKey, EdgeKind);

/// Repo-wide symbol table: name → the definition sites (path, start byte, kind) carrying it.
type DefsByName<'a> = AHashMap<&'a str, Vec<(&'a RelPath, u32, crate::extract::SymbolKind)>>;

/// Stage the name-resolved edge(s) for one import/inherit reference from `from` to `name`. One
/// candidate ⇒ INFERRED; several ⇒ AMBIGUOUS (one edge per candidate); none ⇒ a single INFERRED
/// edge to a virtual `Name` node. Never EXTRACTED — a bare import/inherit is name-level, not a
/// proven binding (ADR-0002). Shared by the imports and inherits lanes.
fn resolve_named_edge(
    push: &mut impl FnMut(NodeKey, NodeKey, EdgeKind, Provenance, u32),
    defs_by_name: &DefsByName<'_>,
    from: NodeKey,
    name: &str,
    kind: EdgeKind,
) {
    match defs_by_name.get(name).filter(|c| !c.is_empty()) {
        None => push(from, NodeKey::Name(name.to_string()), kind, Provenance::Inferred, 1),
        Some(cands) => {
            let prov = if cands.len() > 1 {
                Provenance::Ambiguous
            } else {
                Provenance::Inferred
            };
            for (dp, ds, _k) in cands {
                push(
                    from.clone(),
                    NodeKey::Symbol {
                        path: (*dp).clone(),
                        start_byte: *ds,
                    },
                    kind,
                    prov,
                    1,
                );
            }
        }
    }
}

/// Build the typed, provenance-tagged graph over the current index snapshot + L1 cache.
///
/// `idx = Some` enables proof of call edges via the resolved-reference index; `idx = None`
/// (read-only/degraded serve) still builds every lane but call edges degrade *down* to
/// INFERRED/AMBIGUOUS — never falsely EXTRACTED.
pub(crate) fn build(idx: Option<&IndexDb>, cache: &MapCache, opts: &BuildOpts) -> Result<CodeGraph, McpError> {
    let kinds = opts.kinds;
    let in_focus = |p: &RelPath| {
        opts.focus
            .as_deref()
            .is_none_or(|fx| p.as_str().is_some_and(|s| s.starts_with(fx)))
    };

    // Repo-wide name → definition sites, so a target resolves even when the source file is
    // outside `focus`. `>1` distinct sites for a name is what makes a resolution ambiguous.
    let mut defs_by_name: DefsByName<'_> = AHashMap::new();
    for (path, l1) in &cache.by_path {
        for sym in &l1.symbols {
            defs_by_name
                .entry(sym.name.as_str())
                .or_default()
                .push((path, sym.start_byte, sym.kind));
        }
    }

    let mut acc: AHashMap<EdgeKey, (u32, Provenance)> = AHashMap::new();
    let mut push = |from: NodeKey, to: NodeKey, kind: EdgeKind, prov: Provenance, w: u32| {
        acc.entry((from, to, kind))
            .and_modify(|(weight, p)| {
                *weight += w;
                if prov.rank() > p.rank() {
                    *p = prov;
                }
            })
            .or_insert((w, prov));
    };

    let want_calls = kinds.calls;
    let mut proven_cache: AHashMap<(RelPath, u32), AHashSet<(RelPath, u32)>> = AHashMap::new();
    let mut scanned = 0usize;
    let mut truncated = false;

    for (path, l1) in &cache.by_path {
        if !in_focus(path) {
            continue;
        }

        if kinds.contains {
            for sym in &l1.symbols {
                push(
                    NodeKey::File { path: path.clone() },
                    NodeKey::Symbol {
                        path: path.clone(),
                        start_byte: sym.start_byte,
                    },
                    EdgeKind::Contains,
                    Provenance::Extracted,
                    1,
                );
            }
        }

        if kinds.imports {
            for imp in &l1.imports {
                let Some(leaf) = import_leaf(imp) else { continue };
                resolve_named_edge(
                    &mut push,
                    &defs_by_name,
                    NodeKey::File { path: path.clone() },
                    leaf,
                    EdgeKind::Imports,
                );
            }
        }

        if kinds.inherits {
            for imp in &l1.implementations {
                // The `from` node is the impl site keyed by byte offset; that offset need not match
                // an outline symbol, in which case the node stays identity-consistent but renders
                // with an empty label (cosmetic only — `describe` falls back to the kind).
                let from = NodeKey::Symbol {
                    path: path.clone(),
                    start_byte: imp.start_byte,
                };
                resolve_named_edge(&mut push, &defs_by_name, from, &imp.trait_name, EdgeKind::Inherits);
            }
        }

        if !want_calls {
            continue;
        }

        // Function-like symbols in this file, for attributing a call site to its enclosing
        // definition (innermost by start byte).
        let mut fns: Vec<(u32, u32)> = l1
            .symbols
            .iter()
            .filter(|s| is_function_like(s.kind))
            .map(|s| (s.start_byte, s.end_byte))
            .collect();
        // Sorted by start byte so `enclosing` can binary-search the innermost container instead of
        // scanning every function per call site.
        fns.sort_unstable_by_key(|&(sb, _)| sb);
        let enclosing = |call_byte: u32| -> NodeKey {
            // Among functions starting at or before `call_byte` (the prefix `fns[..hi]`), the
            // innermost container is the one with the largest start byte that also still encloses
            // `call_byte`. Scanning that prefix right-to-left, the first `eb > call_byte` is exactly
            // that max-start container — byte-identical to the old linear max-start scan. A call
            // outside every function (before the first, or in a gap between siblings) matches nothing
            // and falls to the file node; two functions sharing a start byte are interchangeable here
            // since a `Symbol` node's identity is its start byte alone.
            let hi = fns.partition_point(|&(sb, _)| sb <= call_byte);
            let mut best: Option<u32> = None;
            for &(sb, eb) in fns[..hi].iter().rev() {
                if call_byte < eb {
                    best = Some(sb);
                    break;
                }
            }
            match best {
                Some(sb) => NodeKey::Symbol {
                    path: path.clone(),
                    start_byte: sb,
                },
                None => NodeKey::File { path: path.clone() },
            }
        };

        let mut cap_hit = false;
        for_each_call_in_file(idx, cache, path, |callee, call_byte| {
            scanned += 1;
            if scanned > opts.scan_cap {
                cap_hit = true;
                return false;
            }
            // Callees resolve to function-like definitions only (mirrors the call graph).
            let cands: Vec<(&RelPath, u32)> = match defs_by_name.get(callee) {
                Some(c) => c
                    .iter()
                    .filter(|(_, _, k)| is_function_like(*k))
                    .map(|(p, s, _)| (*p, *s))
                    .collect(),
                None => Vec::new(),
            };
            if cands.is_empty() {
                return true; // unresolved call to an out-of-repo symbol — no node to point at
            }
            let base = if cands.len() > 1 {
                Provenance::Ambiguous
            } else {
                Provenance::Inferred
            };
            let from = enclosing(call_byte);

            // A resolved binding names an exact target: if any candidate is proven for this
            // use site, emit only the proven edge(s) as EXTRACTED and drop the rest.
            let mut proven_any = false;
            if let Some(index) = idx {
                for &(dp, ds) in &cands {
                    let uses = proven_cache
                        .entry((dp.clone(), ds))
                        .or_insert_with(|| index.references_to(dp, ds).into_iter().collect());
                    if uses.contains(&(path.clone(), call_byte)) {
                        proven_any = true;
                        push(
                            from.clone(),
                            NodeKey::Symbol {
                                path: dp.clone(),
                                start_byte: ds,
                            },
                            EdgeKind::Calls,
                            Provenance::Extracted,
                            1,
                        );
                    }
                }
            }
            if !proven_any {
                for &(dp, ds) in &cands {
                    push(
                        from.clone(),
                        NodeKey::Symbol {
                            path: dp.clone(),
                            start_byte: ds,
                        },
                        EdgeKind::Calls,
                        base,
                        1,
                    );
                }
            }
            true
        })?;
        if cap_hit {
            truncated = true;
            break;
        }
    }

    let mut edges: Vec<CodeEdge> = acc
        .into_iter()
        .map(|((from, to, kind), (weight, provenance))| CodeEdge {
            from,
            to,
            kind,
            provenance,
            weight,
        })
        .collect();
    edges.sort_by(|a, b| {
        a.kind
            .as_str()
            .cmp(b.kind.as_str())
            .then_with(|| a.from.cmp(&b.from))
            .then_with(|| a.to.cmp(&b.to))
    });

    Ok(CodeGraph { edges, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use tempfile::TempDir;

    /// Scan an inline multi-file repo and return `(tempdir, store, cache)` ready to build a
    /// graph. Mirrors the canonical scan primitive used across the `tests/` suites.
    fn scan_repo(files: &[(&str, &str)]) -> (TempDir, Store, MapCache) {
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for (name, body) in files {
            std::fs::write(root.join(name), body.as_bytes()).expect("write fixture");
        }
        let cfg = crate::config::ConfigV1::with_defaults();
        let mut store = Store::open(root, crate::store::VIEW_WORKING).expect("open store");
        crate::scanner::scan(
            root,
            &mut store,
            &cfg,
            crate::scanner::ScanSource::WorkingTree,
            crate::scanner::EmbedMode::Inline,
        )
        .expect("scan");
        let cache = MapCache::build(&store);
        (dir, store, cache)
    }

    /// A repo exercising every provenance tier + node kind, feature-independent:
    /// - `mod_a.rs` defines `Widget`, a `Drawable` trait + impl, `helper`, `caller` (calls
    ///   `helper` intra-file), and a first `shared`.
    /// - `mod_b.rs` imports a resolvable symbol + an unknown module, defines a second
    ///   `shared`, and calls the now-ambiguous `shared`.
    /// - `mod_c.py` has a Python subclass for a reliable inheritance edge.
    fn provenance_fixture() -> (TempDir, Store, MapCache) {
        scan_repo(&[
            (
                "mod_a.rs",
                "pub struct Widget;\n\
                 pub trait Drawable { fn draw(&self); }\n\
                 impl Drawable for Widget { fn draw(&self) {} }\n\
                 pub fn gadget() {}\n\
                 pub fn helper() {}\n\
                 pub fn caller() { helper(); }\n\
                 pub fn shared() {}\n",
            ),
            (
                "mod_b.rs",
                "use crate::mod_a::gadget;\n\
                 use crate::unknown_mod::Thing;\n\
                 pub fn shared() {}\n\
                 pub fn use_shared() { shared(); }\n",
            ),
            ("mod_c.py", "class Foo:\n    pass\n\nclass Bar(Foo):\n    pass\n"),
        ])
    }

    fn built(store: &Store, cache: &MapCache, kinds: EdgeKindSet) -> CodeGraph {
        build(
            store.index_db.as_ref(),
            cache,
            &BuildOpts {
                kinds,
                focus: None,
                scan_cap: 1_000_000,
            },
        )
        .expect("build codegraph")
    }

    fn name_of(sym_path: &str, key: &NodeKey, cache: &MapCache) -> Option<String> {
        // Resolve a Symbol node back to its symbol name (for readable assertions).
        if let NodeKey::Symbol { path, start_byte } = key
            && path.as_str() == Some(sym_path)
        {
            let l1 = cache.by_path.get(path)?;
            return l1
                .symbols
                .iter()
                .find(|s| s.start_byte == *start_byte)
                .map(|s| s.name.clone());
        }
        None
    }

    #[test]
    fn confidence_ladder_is_fixed() {
        assert_eq!(Provenance::Extracted.confidence(), 1.0);
        assert_eq!(Provenance::Inferred.confidence(), 0.5);
        assert_eq!(Provenance::Ambiguous.confidence(), 0.2);
        assert!(Provenance::Extracted.rank() > Provenance::Inferred.rank());
        assert!(Provenance::Inferred.rank() > Provenance::Ambiguous.rank());
    }

    #[test]
    fn edge_kind_strings_are_stable() {
        assert_eq!(EdgeKind::Calls.as_str(), "calls");
        assert_eq!(EdgeKind::Imports.as_str(), "imports");
        assert_eq!(EdgeKind::Inherits.as_str(), "inherits");
        assert_eq!(EdgeKind::Contains.as_str(), "contains");
    }

    #[test]
    fn edges_param_selects_lanes() {
        let calls = EdgeKindSet::from_edges_param("calls");
        assert!(calls.calls && !calls.imports && !calls.inherits);
        let imports = EdgeKindSet::from_edges_param("imports");
        assert!(imports.imports && !imports.calls);
        let both = EdgeKindSet::from_edges_param("both");
        assert!(both.calls && both.imports && !both.inherits);
        let all = EdgeKindSet::from_edges_param("all");
        assert!(all.calls && all.imports && all.inherits);
        // unknown -> historical default (calls only) ~keep
        let dflt = EdgeKindSet::from_edges_param("bogus");
        assert!(dflt.calls && !dflt.imports && !dflt.inherits);
    }

    #[test]
    fn contains_edges_are_extracted() {
        let (_d, store, cache) = provenance_fixture();
        let g = built(&store, &cache, EdgeKindSet::all());
        let contains: Vec<&CodeEdge> = g.edges.iter().filter(|e| e.kind == EdgeKind::Contains).collect();
        assert!(!contains.is_empty(), "expected file→symbol contains edges");
        for e in &contains {
            assert_eq!(e.provenance, Provenance::Extracted, "contains edges are structural");
            assert_eq!(e.provenance.confidence(), 1.0);
            assert!(matches!(e.from, NodeKey::File { .. }), "contains source is a file node");
            assert!(
                matches!(e.to, NodeKey::Symbol { .. }),
                "contains target is a symbol node"
            );
        }
    }

    #[test]
    fn import_to_known_symbol_is_inferred() {
        let (_d, store, cache) = provenance_fixture();
        let g = built(&store, &cache, EdgeKindSet::all());
        let hit = g
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::Imports && name_of("mod_a.rs", &e.to, &cache).as_deref() == Some("gadget"));
        let hit = hit.expect("import of gadget should resolve to the gadget symbol");
        assert_eq!(
            hit.provenance,
            Provenance::Inferred,
            "name-resolved import is inferred, never extracted"
        );
        assert!(
            matches!(hit.from, NodeKey::File { .. }),
            "import source is the importing file"
        );
    }

    #[test]
    fn import_to_unknown_module_is_a_virtual_name_node() {
        let (_d, store, cache) = provenance_fixture();
        let g = built(&store, &cache, EdgeKindSet::all());
        let hit = g
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::Imports && matches!(&e.to, NodeKey::Name(_)));
        let hit = hit.expect("import of an unknown module yields a virtual Name node");
        assert_ne!(
            hit.provenance,
            Provenance::Extracted,
            "an unresolved import is never extracted"
        );
    }

    #[test]
    fn inherits_resolves_parent_and_is_inferred() {
        let (_d, store, cache) = provenance_fixture();
        let g = built(&store, &cache, EdgeKindSet::all());
        let hit = g
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::Inherits && name_of("mod_c.py", &e.to, &cache).as_deref() == Some("Foo"));
        let hit = hit.expect("Bar(Foo) should produce an inherits edge to Foo");
        assert_eq!(hit.provenance, Provenance::Inferred);
    }

    #[test]
    fn call_to_multi_definition_name_is_ambiguous() {
        let (_d, store, cache) = provenance_fixture();
        let g = built(&store, &cache, EdgeKindSet::all());
        // `shared` is defined in both mod_a.rs and mod_b.rs → any call edge to it is AMBIGUOUS.
        let shared_calls: Vec<&CodeEdge> = g
            .edges
            .iter()
            .filter(|e| {
                e.kind == EdgeKind::Calls
                    && (name_of("mod_a.rs", &e.to, &cache).as_deref() == Some("shared")
                        || name_of("mod_b.rs", &e.to, &cache).as_deref() == Some("shared"))
            })
            .collect();
        assert!(!shared_calls.is_empty(), "expected call edges to `shared`");
        for e in &shared_calls {
            assert_eq!(
                e.provenance,
                Provenance::Ambiguous,
                "a name with >1 definition is ambiguous"
            );
            assert_eq!(e.provenance.confidence(), 0.2);
        }
    }

    #[test]
    fn output_is_deterministic() {
        let (_d, store, cache) = provenance_fixture();
        let a = built(&store, &cache, EdgeKindSet::all());
        let b = built(&store, &cache, EdgeKindSet::all());
        let key = |e: &CodeEdge| {
            (
                e.kind.as_str(),
                format!("{:?}", e.from),
                format!("{:?}", e.to),
                e.provenance.as_str(),
            )
        };
        let ka: Vec<_> = a.edges.iter().map(key).collect();
        let kb: Vec<_> = b.edges.iter().map(key).collect();
        assert_eq!(ka, kb, "graph build must be deterministic across calls");
    }

    #[test]
    fn call_attributes_to_innermost_enclosing_function() {
        // `inner` is nested inside `outer`; the call to `helper` sits in `inner`'s body, so the
        // call edge must originate from the innermost enclosing symbol (`inner`), not `outer`.
        let (_d, store, cache) = scan_repo(&[(
            "nested.rs",
            "pub fn helper() {}\n\
             pub fn outer() {\n\
                 fn inner() { helper(); }\n\
             }\n",
        )]);
        let g = built(&store, &cache, EdgeKindSet::all());
        let call = g
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::Calls && name_of("nested.rs", &e.to, &cache).as_deref() == Some("helper"))
            .expect("a call edge to helper");
        assert_eq!(
            name_of("nested.rs", &call.from, &cache).as_deref(),
            Some("inner"),
            "a call in the nested fn must attribute to the innermost enclosing symbol"
        );
    }

    #[test]
    fn call_outside_every_function_attributes_to_the_file() {
        // The module-scope `helper()` call is enclosed by no function (it precedes `outer`), so it
        // falls to the file node; the call inside `outer` attributes to `outer`. Guards the
        // `enclosing` fallback path the partition-point rewrite must preserve.
        let (_d, store, cache) = scan_repo(&[(
            "m.py",
            "def helper():\n    pass\n\nhelper()\n\ndef outer():\n    helper()\n",
        )]);
        let g = built(&store, &cache, EdgeKindSet::all());
        let calls: Vec<&CodeEdge> = g
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls && name_of("m.py", &e.to, &cache).as_deref() == Some("helper"))
            .collect();
        assert!(
            calls.iter().any(|e| matches!(e.from, NodeKey::File { .. })),
            "the module-scope call must attribute to the file node, not a function"
        );
        assert!(
            calls
                .iter()
                .any(|e| name_of("m.py", &e.from, &cache).as_deref() == Some("outer")),
            "the in-function call must attribute to outer"
        );
    }

    /// Cross-file/intra-file resolved call proof (EXTRACTED) is only available when a
    /// precise-resolution feature is compiled in. Under default features call edges degrade
    /// down to INFERRED — which the other tests already cover — so this proof is gated.
    #[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
    #[test]
    fn resolved_calls_are_extracted_with_intel() {
        use std::path::Path;
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/precise_resolution_py");
        for entry in std::fs::read_dir(&src).expect("fixture dir") {
            let entry = entry.expect("dir entry");
            let name = entry.file_name().to_string_lossy().to_string();
            std::fs::copy(entry.path(), root.join(&name)).expect("copy fixture");
        }
        let cfg = crate::config::ConfigV1::with_defaults();
        let mut store = Store::open(root, crate::store::VIEW_WORKING).expect("open store");
        crate::scanner::scan(
            root,
            &mut store,
            &cfg,
            crate::scanner::ScanSource::WorkingTree,
            crate::scanner::EmbedMode::Inline,
        )
        .expect("scan");
        let cache = MapCache::build(&store);
        let g = built(&store, &cache, EdgeKindSet::all());
        let has_extracted_call = g
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::Calls && e.provenance == Provenance::Extracted);
        assert!(
            has_extracted_call,
            "a resolved cross-file call should be EXTRACTED under intel features"
        );
    }

    fn opts(kinds: EdgeKindSet) -> BuildOpts {
        BuildOpts {
            kinds,
            focus: None,
            scan_cap: CODEGRAPH_SCAN_CAP,
        }
    }

    #[test]
    fn memo_serves_the_same_build_for_one_snapshot() {
        let (_dir, store, cache) = provenance_fixture();
        let idx = store.index_db.as_ref();
        let memo = Mutex::new(new_graph_memo());

        let first = build_memoized(&memo, idx, &cache, &opts(EdgeKindSet::all())).expect("first build");
        let second = build_memoized(&memo, idx, &cache, &opts(EdgeKindSet::all())).expect("second build");
        assert!(
            Arc::ptr_eq(&first, &second),
            "same cache + key must return the cached Arc"
        );

        // The cached graph matches a fresh, un-memoized build.
        let fresh = built(&store, &cache, EdgeKindSet::all());
        assert_eq!(
            first.edges.len(),
            fresh.edges.len(),
            "memoized graph must equal a direct build"
        );
    }

    #[test]
    fn memo_keys_on_lanes_and_index_mode() {
        let (_dir, store, cache) = provenance_fixture();
        let idx = store.index_db.as_ref();
        let memo = Mutex::new(new_graph_memo());

        let calls_only = EdgeKindSet {
            calls: true,
            ..EdgeKindSet::none()
        };
        let all = build_memoized(&memo, idx, &cache, &opts(EdgeKindSet::all())).expect("all lanes");
        let calls = build_memoized(&memo, idx, &cache, &opts(calls_only)).expect("calls lane");
        assert!(
            !Arc::ptr_eq(&all, &calls),
            "distinct lane sets are distinct cache entries"
        );

        // idx presence is part of the key: the same lanes without a live index build a distinct entry
        // (call edges degrade to a different provenance tier).
        let no_idx = build_memoized(&memo, None, &cache, &opts(EdgeKindSet::all())).expect("no-index build");
        assert!(
            !Arc::ptr_eq(&all, &no_idx),
            "idx=Some and idx=None are distinct cache entries"
        );

        // Every key coexists in the LRU (cap 16 » 3): re-fetching returns each one's own cached Arc.
        let all_again = build_memoized(&memo, idx, &cache, &opts(EdgeKindSet::all())).expect("all lanes again");
        assert!(
            Arc::ptr_eq(&all, &all_again),
            "the all-lanes entry survives intervening distinct-key builds"
        );
    }

    #[test]
    fn memo_isolates_snapshots_by_fingerprint() {
        // Two repos with different content have different cache fingerprints, so a build over one
        // never serves the other's graph, and each keeps its own entry (proving the fingerprint is
        // part of the key — a stale snapshot cannot masquerade as a fresh one).
        let (_da, store_a, cache_a) = scan_repo(&[("m.rs", "pub fn a() {}\npub fn caller() { a(); }\n")]);
        let (_db, store_b, cache_b) =
            scan_repo(&[("m.rs", "pub fn a() {}\npub fn b() {}\npub fn caller() { a(); b(); }\n")]);
        assert_ne!(
            cache_a.fingerprint, cache_b.fingerprint,
            "different content must fingerprint differently"
        );
        let memo = Mutex::new(new_graph_memo());

        let ga = build_memoized(&memo, store_a.index_db.as_ref(), &cache_a, &opts(EdgeKindSet::all())).expect("a");
        let gb = build_memoized(&memo, store_b.index_db.as_ref(), &cache_b, &opts(EdgeKindSet::all())).expect("b");
        assert!(!Arc::ptr_eq(&ga, &gb), "distinct fingerprints are distinct entries");

        let ga_again =
            build_memoized(&memo, store_a.index_db.as_ref(), &cache_a, &opts(EdgeKindSet::all())).expect("a again");
        assert!(
            Arc::ptr_eq(&ga, &ga_again),
            "snapshot A still hits its own entry after B was inserted"
        );
    }
}
