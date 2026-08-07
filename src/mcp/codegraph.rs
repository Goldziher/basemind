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

use ahash::AHashMap;
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
    /// A rationale marker (WHY/NOTE/TODO/…) attached to the code it annotates (ADR-0009).
    Annotates,
    /// A rationale marker citing a decision record (ADR/RFC) (ADR-0009).
    Cites,
    /// A document chunk mentioning a code symbol / file — the doc↔code lane (ADR-0008).
    #[cfg(feature = "documents")]
    Documents,
}

impl EdgeKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Calls => "calls",
            EdgeKind::Imports => "imports",
            EdgeKind::Inherits => "inherits",
            EdgeKind::Contains => "contains",
            EdgeKind::Annotates => "annotates",
            EdgeKind::Cites => "cites",
            #[cfg(feature = "documents")]
            EdgeKind::Documents => "documents",
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
    Symbol {
        path: RelPath,
        start_byte: u32,
    },
    File {
        path: RelPath,
    },
    Name(String),
    /// A rationale marker span in a source file — a WHY/NOTE/TODO/… note promoted to a node
    /// (ADR-0009). The source end of `Annotates` / `Cites` edges.
    Rationale {
        path: RelPath,
        start_byte: u32,
    },
    /// A decision record (an ADR / RFC file) — the target of a `Cites` edge (ADR-0009).
    Decision {
        path: RelPath,
    },
    /// A chunk of an extracted document — the source of a `Documents` edge (ADR-0008). Identified by
    /// its owning document path plus the 0-based chunk index within that document.
    #[cfg(feature = "documents")]
    DocChunk {
        path: RelPath,
        chunk_idx: u32,
    },
}

impl NodeKey {
    /// The owning file path, when the node has one. `None` only for a virtual `Name` node.
    /// Consumers that aggregate to file/module granularity drop `None` endpoints — so the
    /// document / rationale / decision nodes carry their file to stay visible in those views.
    pub(crate) fn file(&self) -> Option<&RelPath> {
        match self {
            NodeKey::Symbol { path, .. }
            | NodeKey::File { path }
            | NodeKey::Rationale { path, .. }
            | NodeKey::Decision { path } => Some(path),
            #[cfg(feature = "documents")]
            NodeKey::DocChunk { path, .. } => Some(path),
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

/// A raw mention extracted from a document chunk (ADR-0008). Resolution to a graph node is deferred
/// to the `documents` build lane: a [`Name`](DocMention::Name) resolves against the repo symbol
/// table, a [`Path`](DocMention::Path) against the indexed file set.
#[cfg(feature = "documents")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DocMention {
    /// An identifier / keyword / entity mention — resolved by name (INFERRED / AMBIGUOUS).
    Name(String),
    /// An explicit repo-relative path citation — resolved to a file node (EXTRACTED when indexed).
    Path(RelPath),
}

/// One persisted document→code link (ADR-0008): a chunk of `doc_path` mentioning `mention`. Produced
/// at document-scan time, stored in the LanceDB document store, and reloaded into the [`MapCache`] by
/// the async cache-warm path; the `documents` build lane turns each into a typed `Documents` edge.
#[cfg(feature = "documents")]
#[derive(Debug, Clone)]
pub(crate) struct DocLink {
    /// Repo-relative path of the source document.
    pub(crate) doc_path: RelPath,
    /// 0-based chunk index within the document.
    pub(crate) chunk_idx: u32,
    /// The raw mention this chunk carries.
    pub(crate) mention: DocMention,
}

/// Which edge lanes to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct EdgeKindSet {
    pub(crate) calls: bool,
    pub(crate) imports: bool,
    pub(crate) inherits: bool,
    pub(crate) contains: bool,
    /// Rationale→code annotation edges (ADR-0009). Opt-in — off in `from_edges_param`'s `"all"`.
    pub(crate) annotates: bool,
    /// Rationale→decision citation edges (ADR-0009). Opt-in.
    pub(crate) cites: bool,
    /// Document→code edges (ADR-0008). Opt-in — off in `from_edges_param`'s `"all"`. The field is
    /// ungated so the derived `Hash`/`Eq` (this type is a `GraphMemo` key) keep it live on the
    /// default build; the lane it drives is only ever set under `feature = "documents"`.
    pub(crate) documents: bool,
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
            annotates: true,
            cites: true,
            documents: true,
        }
    }

    fn none() -> Self {
        Self {
            calls: false,
            imports: false,
            inherits: false,
            contains: false,
            annotates: false,
            cites: false,
            documents: false,
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
            EdgeKind::Annotates => self.annotates,
            EdgeKind::Cites => self.cites,
            #[cfg(feature = "documents")]
            EdgeKind::Documents => self.documents,
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
    pub(crate) focus: Option<RelPath>,
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
pub(crate) type GraphKey = (u64, EdgeKindSet, Option<RelPath>, bool);

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

/// The canonical form of a decision-record citation id: `ADR-0007`, `RFC-2119` — an uppercase
/// kind prefix and a zero-padded (min 4) number. Both the rationale extractor (which fills
/// [`crate::extract::RationaleRecord::citations`]) and [`decision_id_of_path`] MUST produce this
/// exact shape so a citation resolves to its decision file (ADR-0009).
pub(crate) fn normalize_decision_id(prefix: &str, number: u32) -> String {
    format!("{}-{number:04}", prefix.to_ascii_uppercase())
}

/// The decision-record id a file path denotes, if it is one — a file whose name begins with digits
/// inside an `adr/` or `rfc/` directory (e.g. `docs/adr/0007-graph.md` → `ADR-0007`). `None` for an
/// ordinary source file. Used to resolve rationale citations to [`NodeKey::Decision`] nodes.
fn decision_id_of_path(path: &RelPath) -> Option<String> {
    let s = path.as_str()?;
    // A decision record lives under an `adr/` or `rfc/` directory, so it always has a `/`; split off
    // the filename and scan the directory segments without allocating a `Vec`.
    let (dir, base) = s.rsplit_once('/')?;
    let in_adr = dir.split('/').any(|seg| seg.eq_ignore_ascii_case("adr"));
    let in_rfc = dir.split('/').any(|seg| seg.eq_ignore_ascii_case("rfc"));
    if !in_adr && !in_rfc {
        return None;
    }
    let digits = base.as_bytes().iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let number: u32 = base[..digits].parse().ok()?;
    Some(normalize_decision_id(if in_adr { "ADR" } else { "RFC" }, number))
}

/// The nearest code symbol to a rationale marker at byte `marker`, as a start byte. Prefers the
/// innermost symbol whose span *contains* the marker (an inline note); failing that, the closest
/// symbol that *starts after* the marker (a doc-comment above a definition). `None` ⇒ no symbol is
/// near, so the note attaches to the file node. `syms_by_start` must be sorted by start byte.
fn attach_symbol(syms_by_start: &[(u32, u32)], marker: u32) -> Option<u32> {
    let hi = syms_by_start.partition_point(|&(sb, _)| sb <= marker);
    for &(sb, eb) in syms_by_start[..hi].iter().rev() {
        if marker < eb {
            return Some(sb);
        }
    }
    syms_by_start[hi..].first().map(|&(sb, _)| sb)
}

/// Accumulator key: an edge is unique by `(from, to, kind)`; weight sums and provenance
/// folds to the strongest tier across duplicates.
type EdgeKey = (NodeKey, NodeKey, EdgeKind);

/// Repo-wide symbol table: name → the definition sites (path, byte span, kind) carrying it.
type DefsByName<'a> = AHashMap<&'a str, Vec<(&'a RelPath, u32, u32, crate::extract::SymbolKind)>>;

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
            for (dp, ds, _de, _k) in cands {
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
    // Compared byte-wise, not as `&str`: for UTF-8 paths a byte prefix and a `str` prefix are the
    // same test, and matching on bytes additionally keeps non-UTF-8 indexed paths in scope instead
    // of dropping them from every focused build. ~keep
    let in_focus = |p: &RelPath| {
        opts.focus
            .as_ref()
            .is_none_or(|fx| p.as_bytes().starts_with(fx.as_bytes()))
    };

    // Repo-wide name → definition sites, so a target resolves even when the source file is
    // outside `focus`. `>1` distinct sites for a name is what makes a resolution ambiguous.
    let mut defs_by_name: DefsByName<'_> = AHashMap::new();
    for (path, l1) in &cache.by_path {
        for sym in &l1.symbols {
            defs_by_name
                .entry(sym.name.as_str())
                .or_default()
                .push((path, sym.start_byte, sym.end_byte, sym.kind));
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
                // The `from` node is the impl site keyed by byte offset; that offset need not match ~keep
                // an outline symbol, in which case the node stays identity-consistent but renders ~keep
                // with an empty label (cosmetic only — `describe` falls back to the kind). ~keep
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
            let cands: Vec<(&RelPath, u32, u32)> = match defs_by_name.get(callee) {
                Some(c) => c
                    .iter()
                    .filter(|(_, _, _, k)| is_function_like(*k))
                    .map(|(p, start, end, _)| (*p, *start, *end))
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

            // Resolver definition offsets point at the identifier, while L1 symbol offsets point
            // at the whole definition node (`def`, `fn`, `function`, ...). Match the resolved byte
            // against each candidate's span and keep the graph node keyed by its stable L1 start.
            let proven = idx
                .and_then(|index| index.definition_of(path, call_byte))
                .and_then(|(def_path, def_byte)| {
                    cands
                        .iter()
                        .filter(|(candidate_path, start, end)| {
                            **candidate_path == def_path && def_byte >= *start && def_byte < *end
                        })
                        .max_by_key(|(_, start, _)| *start)
                        .copied()
                });
            if let Some((dp, ds, _)) = proven {
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
            if proven.is_none() {
                for &(dp, ds, _) in &cands {
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

    // ── Rationale lane (ADR-0009): promote WHY/NOTE/… notes to nodes, attach them to the code
    // they annotate (proximity ⇒ INFERRED), and cite decision records they reference (a resolved
    // ADR/RFC file ⇒ EXTRACTED; an unresolved citation ⇒ a virtual `Name` node, INFERRED).
    if kinds.annotates || kinds.cites {
        let mut decisions_by_id: AHashMap<String, Vec<&RelPath>> = AHashMap::new();
        if kinds.cites {
            for path in cache.by_path.keys() {
                if let Some(id) = decision_id_of_path(path) {
                    decisions_by_id.entry(id).or_default().push(path);
                }
            }
        }
        for (path, l1) in &cache.by_path {
            if !in_focus(path) || l1.rationale.is_empty() {
                continue;
            }
            let mut syms: Vec<(u32, u32)> = l1.symbols.iter().map(|s| (s.start_byte, s.end_byte)).collect();
            syms.sort_unstable_by_key(|&(sb, _)| sb);
            for rec in &l1.rationale {
                let from = NodeKey::Rationale {
                    path: path.clone(),
                    start_byte: rec.start_byte,
                };
                if kinds.annotates {
                    let target = match attach_symbol(&syms, rec.start_byte) {
                        Some(sb) => NodeKey::Symbol {
                            path: path.clone(),
                            start_byte: sb,
                        },
                        None => NodeKey::File { path: path.clone() },
                    };
                    push(from.clone(), target, EdgeKind::Annotates, Provenance::Inferred, 1);
                }
                if kinds.cites {
                    for citation in &rec.citations {
                        match decisions_by_id.get(citation.as_str()).filter(|c| !c.is_empty()) {
                            // Resolved to exactly one decision file ⇒ EXTRACTED; a colliding id that
                            // two files both claim ⇒ AMBIGUOUS (one edge each), mirroring the other lanes.
                            Some(paths) => {
                                let prov = if paths.len() > 1 {
                                    Provenance::Ambiguous
                                } else {
                                    Provenance::Extracted
                                };
                                for dp in paths {
                                    push(
                                        from.clone(),
                                        NodeKey::Decision { path: (*dp).clone() },
                                        EdgeKind::Cites,
                                        prov,
                                        1,
                                    );
                                }
                            }
                            None => push(
                                from.clone(),
                                NodeKey::Name(citation.clone()),
                                EdgeKind::Cites,
                                Provenance::Inferred,
                                1,
                            ),
                        }
                    }
                }
            }
        }
    }

    // ── Document lane (ADR-0008): promote each persisted document→code link to a `DocChunk` node and
    // resolve its mention. A name mention resolves through the shared symbol table (INFERRED /
    // AMBIGUOUS / a virtual `Name` node); a path citation points at the file node — EXTRACTED when the
    // cited file is indexed, INFERRED otherwise.
    #[cfg(feature = "documents")]
    if kinds.documents {
        for link in cache.doc_links.iter() {
            if !in_focus(&link.doc_path) {
                continue;
            }
            let from = NodeKey::DocChunk {
                path: link.doc_path.clone(),
                chunk_idx: link.chunk_idx,
            };
            match &link.mention {
                DocMention::Name(name) => resolve_named_edge(&mut push, &defs_by_name, from, name, EdgeKind::Documents),
                DocMention::Path(target) => {
                    if cache.by_path.contains_key(target) {
                        push(
                            from,
                            NodeKey::File { path: target.clone() },
                            EdgeKind::Documents,
                            Provenance::Extracted,
                            1,
                        );
                    } else {
                        // Unresolved path: don't invent a `File` node for a path that was never
                        // indexed — mirror the Name/import lane and point at a virtual `Name` node.
                        let name = target.as_str().unwrap_or_default().to_string();
                        push(from, NodeKey::Name(name), EdgeKind::Documents, Provenance::Inferred, 1);
                    }
                }
            }
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
#[path = "codegraph_tests.rs"]
mod tests;
