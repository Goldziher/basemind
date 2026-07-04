//! Intra-file scope resolution via tree-sitter `locals` queries.
//!
//! This is Phase 1 of the code-intelligence tier: make name-based navigation scope-aware
//! *within a single file*. tree-sitter grammars ship a `locals.scm` query using the standard
//! capture convention — `@local.scope` marks a lexical scope, `@local.definition[.*]` marks a
//! binding, `@local.reference` marks a use. This module compiles that query (from a vendored
//! override or straight from TSLP) and resolves each reference to the nearest enclosing
//! definition of the same name.
//!
//! The output ([`LocalBindings`]) answers "does the identifier at this byte offset bind to a
//! definition in *this* file, and where?" — which lets `find_references` / `find_callers`
//! stop conflating a locally-bound `bar` with same-named symbols in other files.
//!
//! ## Layering
//!
//! The scope-resolution algorithm ([`resolve_bindings`]) is deliberately separated from the
//! tree-sitter query walk ([`build_bindings`]) so it can be unit-tested with synthetic
//! scope/definition/reference data — no grammar download required. Only the thin walk wrapper
//! needs a real grammar.

use std::sync::{Arc, OnceLock, RwLock};

use ahash::AHashMap;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor, Tree};

use crate::lang::{self, LangError, LangId};

/// Vendored `locals.scm` for override languages TSLP ships no usable upstream locals for
/// (python, typescript, tsx, go — empirically confirmed `None` from `get_locals_query` in the
/// 1.12.3 bundle; only `javascript` ships one). Each follows the standard tree-sitter locals
/// capture convention so [`build_bindings`] treats them identically to upstream queries. Returns
/// a `&'static str` via `include_str!` — no runtime allocation. Any language not listed here
/// falls through to the raw TSLP `get_locals_query`.
fn vendored_locals(lang: LangId) -> Option<&'static str> {
    match lang {
        "python" => Some(include_str!("../queries/python-locals.scm")),
        "typescript" => Some(include_str!("../queries/typescript-locals.scm")),
        "tsx" => Some(include_str!("../queries/tsx-locals.scm")),
        "go" => Some(include_str!("../queries/go-locals.scm")),
        _ => None,
    }
}

type CachedQuery = Option<Arc<Query>>;
static LOCALS_QUERIES: OnceLock<RwLock<AHashMap<LangId, CachedQuery>>> = OnceLock::new();

/// Compile + cache the `locals` query for a language.
///
/// Prefers a vendored override (see [`vendored_locals`]), else the raw TSLP `locals.scm` via
/// `get_locals_query`. Compiled with basemind's own tree-sitter runtime — deliberately *not*
/// TSLP's compiled `get_query(_, Locals)`, which binds the query to TSLP's bundled runtime
/// version and would break under a runtime-version skew (the crate documents this caveat).
///
/// Returns `Ok(None)` when the language ships no locals query at all (the resolver then yields
/// empty bindings and callers fall back to today's name-based behavior). The negative result is
/// cached to skip the lookup on every subsequent file of that language.
pub fn locals_query(lang: LangId) -> Result<CachedQuery, LangError> {
    let lock = LOCALS_QUERIES.get_or_init(|| RwLock::new(AHashMap::new()));
    if let Some(slot) = lock.read().expect("locals query pool poisoned").get(&lang) {
        return Ok(slot.as_ref().map(Arc::clone));
    }
    let source: Option<&'static str> =
        vendored_locals(lang).or_else(|| tree_sitter_language_pack::get_locals_query(lang));
    let cached = match source {
        Some(src) => {
            let ts_lang = lang::language(lang)?;
            let query = Query::new(&ts_lang, src).map_err(|e| LangError::QueryCompile {
                lang,
                kind: "locals",
                msg: format!("{e}"),
            })?;
            Some(Arc::new(query))
        }
        None => None,
    };
    lock.write()
        .expect("locals query pool poisoned")
        .insert(lang, cached.as_ref().map(Arc::clone));
    Ok(cached)
}

/// The definition span (and the reference's own end byte) a resolved reference binds to.
///
/// Kept `Copy` and value-sized so the binding map stays a flat `AHashMap<u32, ResolvedSpan>` with
/// no per-reference heap allocation. All three ends come straight from the tree-sitter capture
/// nodes — no extra tree walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResolvedSpan {
    /// End byte of the reference identifier (its start is the map key).
    use_end: u32,
    /// Start byte of the bound definition identifier.
    def_start: u32,
    /// End byte of the bound definition identifier.
    def_end: u32,
}

/// Intra-file scope-resolution result.
///
/// Maps a reference identifier's start byte to the nearest enclosing local definition of the same
/// name, carrying both identifiers' end bytes so callers can recover real spans (name extraction,
/// span-containment). References that bind to no in-file definition (module-imported or otherwise
/// free names) are simply absent — callers treat those as cross-file candidates.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LocalBindings {
    ref_to_def: AHashMap<u32, ResolvedSpan>,
}

impl LocalBindings {
    /// Start byte of the in-file definition the reference at `ref_start_byte` binds to, if any.
    pub fn resolved_def(&self, ref_start_byte: u32) -> Option<u32> {
        self.ref_to_def.get(&ref_start_byte).map(|span| span.def_start)
    }

    /// True when the reference at `ref_start_byte` binds to a definition in this file.
    pub fn is_local(&self, ref_start_byte: u32) -> bool {
        self.ref_to_def.contains_key(&ref_start_byte)
    }

    /// Iterate the resolved `(use_start_byte, def_start_byte)` edges. Order is unspecified.
    pub fn edges(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.ref_to_def
            .iter()
            .map(|(&use_start, span)| (use_start, span.def_start))
    }

    /// Iterate the resolved edges with full spans: `(use_start, use_end, def_start, def_end)`.
    /// The end bytes come from the tree-sitter capture nodes, so both spans are real identifier
    /// extents — unlike the interim zero-width ends the locals path used to emit. Order is
    /// unspecified.
    pub fn edges_spanned(&self) -> impl Iterator<Item = (u32, u32, u32, u32)> + '_ {
        self.ref_to_def
            .iter()
            .map(|(&use_start, span)| (use_start, span.use_end, span.def_start, span.def_end))
    }

    pub fn is_empty(&self) -> bool {
        self.ref_to_def.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ref_to_def.len()
    }
}

/// Resolve intra-file local bindings for a parsed tree using the language's `locals` query.
///
/// Returns empty bindings when the language ships no locals query (the common
/// data-only-format case, and any grammar without a `locals.scm`).
pub fn resolve_locals(lang: LangId, tree: &Tree, source: &[u8]) -> Result<LocalBindings, LangError> {
    let Some(query) = locals_query(lang)? else {
        return Ok(LocalBindings::default());
    };
    Ok(build_bindings(&query, tree.root_node(), source))
}

/// Walk the compiled locals query over `root`, collect scopes/definitions/references, and hand
/// them to the pure [`resolve_bindings`] core. Kept thin so the resolution logic stays
/// grammar-independent and unit-testable.
fn build_bindings(query: &Query, root: Node, source: &[u8]) -> LocalBindings {
    let names = query.capture_names();
    debug_assert!(source.len() <= u32::MAX as usize, "source exceeds u32 byte range");
    let src_len = source.len() as u32;

    // index 0 is always the implicit whole-file root scope.
    let mut scopes: Vec<(u32, u32)> = vec![(0, src_len)];
    // (name bytes borrowed from source, start_byte, end_byte)
    let mut defs: Vec<(&[u8], u32, u32)> = Vec::new();
    let mut refs: Vec<(&[u8], u32, u32)> = Vec::new();

    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(query, root, source);
    while let Some(m) = iter.next() {
        for cap in m.captures {
            let cname = names[cap.index as usize];
            let node = cap.node;
            let start = node.start_byte() as u32;
            let end = node.end_byte() as u32;
            if cname == "local.scope" {
                scopes.push((start, end));
            } else if cname.starts_with("local.definition") {
                let name = &source[node.start_byte()..node.end_byte()];
                defs.push((name, start, end));
            } else if cname.starts_with("local.reference") {
                let name = &source[node.start_byte()..node.end_byte()];
                refs.push((name, start, end));
            }
        }
    }

    LocalBindings {
        ref_to_def: resolve_bindings(&scopes, &defs, &refs),
    }
}

/// Pure scope-resolution core: bind each reference to the nearest enclosing definition of the
/// same name. Grammar-independent — operates only on byte ranges and identifier bytes.
///
/// `scopes[0]` is assumed to be the whole-file root scope. Each definition is owned by the
/// innermost scope that contains it; a reference resolves by walking its own containing-scope
/// chain innermost-first and taking the first scope that owns a same-named definition. Binding
/// to the root scope (a module-level definition) still counts — the signal is "defined in this
/// file", which is what cross-file filtering needs.
///
/// Two deliberate simplifications, correct for the "is this name local to the file?" goal but
/// NOT for precise scope-exact goto-definition:
/// - **Source order is not enforced** — a reference may bind to a *later* definition in the same
///   scope (harmless for hoisted names; imprecise for `let`/`const` temporal-dead-zone cases).
/// - **`local.scope-inherits` predicates are ignored** — every scope inherits from all ancestors,
///   so an isolated scope (e.g. a grammar marking `(#set! local.scope-inherits false)`) may
///   over-resolve to an outer binding. Both are acceptable because over-resolving still yields an
///   in-file binding; JS/TS precise resolution goes through the oxc engine, not this path.
fn resolve_bindings(
    scopes: &[(u32, u32)],
    defs: &[(&[u8], u32, u32)],
    refs: &[(&[u8], u32, u32)],
) -> AHashMap<u32, ResolvedSpan> {
    // Each definition's owning scope = the innermost scope containing its range.
    let mut defs_by_scope: AHashMap<usize, Vec<usize>> = AHashMap::new();
    for (di, &(_, ds, de)) in defs.iter().enumerate() {
        let owner = innermost_scope(scopes, ds, de);
        defs_by_scope.entry(owner).or_default().push(di);
    }

    // Scope indices sorted innermost-first (smallest area), once — reused for every reference so
    // the per-ref containment walk is an allocation-free linear scan instead of a per-ref
    // collect-and-sort.
    let mut scope_order: Vec<usize> = (0..scopes.len()).collect();
    // `saturating_sub`: a well-formed grammar always emits `end >= start`, but a malformed one could
    // invert the span; saturating keeps the sort total (area 0 → treated as innermost) rather than
    // wrapping to a huge value in release builds.
    scope_order.sort_by_key(|&i| scopes[i].1.saturating_sub(scopes[i].0));

    let mut ref_to_def: AHashMap<u32, ResolvedSpan> = AHashMap::new();
    for &(rname, rs, re) in refs {
        for &sc in &scope_order {
            let (s, e) = scopes[sc];
            if s <= rs
                && re <= e
                && let Some(dis) = defs_by_scope.get(&sc)
                && let Some(&di) = dis.iter().find(|&&di| defs[di].0 == rname)
            {
                let (_, def_start, def_end) = defs[di];
                ref_to_def.insert(
                    rs,
                    ResolvedSpan {
                        use_end: re,
                        def_start,
                        def_end,
                    },
                );
                break;
            }
        }
    }
    ref_to_def
}

/// Index of the innermost scope (smallest area) that contains the byte range `[start, end]`.
/// Falls back to the root scope (index 0), which contains everything.
fn innermost_scope(scopes: &[(u32, u32)], start: u32, end: u32) -> usize {
    debug_assert!(!scopes.is_empty(), "scopes must contain at least the root scope");
    let mut best = 0usize;
    let mut best_area = scopes[0].1 - scopes[0].0;
    for (i, &(s, e)) in scopes.iter().enumerate() {
        if s <= start && end <= e {
            let area = e - s;
            if area < best_area {
                best = i;
                best_area = area;
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic layout mirroring:
    //   function outer(a) {            // scope S1 = bytes [10, 100]
    //     let x = 1;                   // def x @ 30
    //     function inner(b) {          // scope S2 = bytes [50, 90], def inner @ 45, def b @ 60
    //       return x + b;              // ref x @ 70, ref b @ 74
    //     }
    //     return inner(a);             // ref inner @ 95, ref a @ 96
    //   }
    // Root scope [0, 200]. a is a param owned by S1.
    type NamedSpan = (&'static [u8], u32, u32);
    type Fixture = (Vec<(u32, u32)>, Vec<NamedSpan>, Vec<NamedSpan>);
    fn fixture() -> Fixture {
        let scopes = vec![(0u32, 200u32), (10, 100), (50, 90)];
        let defs: Vec<(&[u8], u32, u32)> = vec![
            (b"a".as_slice(), 20, 21),
            (b"x".as_slice(), 30, 31),
            (b"inner".as_slice(), 45, 50),
            (b"b".as_slice(), 60, 61),
        ];
        let refs: Vec<(&[u8], u32, u32)> = vec![
            (b"x".as_slice(), 70, 71),
            (b"b".as_slice(), 74, 75),
            (b"inner".as_slice(), 95, 100),
            (b"a".as_slice(), 96, 97),
        ];
        (scopes, defs, refs)
    }

    #[test]
    fn reference_binds_to_nearest_enclosing_definition() {
        let (scopes, defs, refs) = fixture();
        let map = resolve_bindings(&scopes, &defs, &refs);
        // x (in inner) binds to the outer `let x` at 30..31, ref span 70..71.
        let x = map.get(&70).expect("x should bind");
        assert_eq!(
            (x.def_start, x.def_end),
            (30, 31),
            "x should bind to outer let x@30..31"
        );
        assert_eq!(x.use_end, 71, "x ref end must be the identifier end, not the start");
        // b binds to inner's param at 60..61.
        assert_eq!(
            map.get(&74).map(|s| s.def_start),
            Some(60),
            "b should bind to inner param b@60"
        );
        // inner (called in outer) binds to the function def at 45..50, ref span 95..100.
        let inner = map.get(&95).expect("inner should bind");
        assert_eq!(
            (inner.def_start, inner.def_end),
            (45, 50),
            "inner should bind to function inner@45..50"
        );
        assert_eq!(inner.use_end, 100, "inner ref end must be the identifier end");
        // a binds to outer's param at 20.
        assert_eq!(
            map.get(&96).map(|s| s.def_start),
            Some(20),
            "a should bind to outer param a@20"
        );
    }

    #[test]
    fn shadowing_prefers_inner_scope() {
        // Same name `v` defined at both the outer scope (owned by S1) and the inner scope
        // (owned by S2). A reference inside S2 must bind to the inner definition, not the outer.
        let scopes = vec![(0u32, 200u32), (10, 100), (50, 90)];
        let defs: Vec<(&[u8], u32, u32)> = vec![(b"v".as_slice(), 30, 31), (b"v".as_slice(), 55, 56)];
        let refs: Vec<(&[u8], u32, u32)> = vec![(b"v".as_slice(), 70, 71)];
        let map = resolve_bindings(&scopes, &defs, &refs);
        assert_eq!(
            map.get(&70).map(|s| s.def_start),
            Some(55),
            "inner v@55 must shadow outer v@30"
        );
    }

    #[test]
    fn unbound_reference_is_absent() {
        // A reference to a name with no matching definition anywhere (an imported/global symbol)
        // must NOT resolve — callers treat it as a cross-file candidate.
        let (scopes, defs, _) = fixture();
        let refs: Vec<(&[u8], u32, u32)> = vec![(b"fetch".as_slice(), 70, 75)];
        let map = resolve_bindings(&scopes, &defs, &refs);
        assert!(map.get(&70).is_none(), "free name `fetch` must stay unresolved");
        assert!(!LocalBindings { ref_to_def: map }.is_local(70));
    }

    #[test]
    fn module_level_binding_resolves_to_root() {
        // A top-level def (owned by the root scope) still binds a same-file reference.
        let scopes = vec![(0u32, 200u32), (10, 100)];
        let defs: Vec<(&[u8], u32, u32)> = vec![(b"helper".as_slice(), 5, 11)];
        let refs: Vec<(&[u8], u32, u32)> = vec![(b"helper".as_slice(), 50, 56)];
        let map = resolve_bindings(&scopes, &defs, &refs);
        assert_eq!(
            map.get(&50).map(|s| s.def_start),
            Some(5),
            "root-level helper must still bind"
        );
    }

    /// Parse `bytes` with `lang`'s grammar, returning the tree — or `None` when the grammar (or
    /// its download) is unavailable in this environment, so grammar-gated tests can skip cleanly.
    fn try_parse(lang: LangId, bytes: &[u8]) -> Option<Tree> {
        use crate::lang::{self, ParseOutcome};
        // A vendored/compiled locals query is a prerequisite for a meaningful assertion.
        if !matches!(locals_query(lang), Ok(Some(_))) {
            return None;
        }
        match lang::with_parser(lang, |p| lang::parse_with_default_timeout(p, bytes)) {
            Ok(ParseOutcome::Ok(tree)) => Some(tree),
            _ => None,
        }
    }

    /// The full spanned edge `(use_start, use_end, def_start, def_end)` for the reference that
    /// starts at `use_start`, if it resolved. Used to assert the enriched end offsets.
    fn spanned_for(bindings: &LocalBindings, use_start: u32) -> Option<(u32, u32, u32, u32)> {
        bindings.edges_spanned().find(|&(us, ..)| us == use_start)
    }

    #[test]
    fn resolve_locals_binds_python_local() {
        // `x` and param `a` used inside `outer` must bind to the in-file local defs, not a global.
        let lang = "python";
        let src = "def outer(a):\n    x = 1\n    return x + a\n";
        let bytes = src.as_bytes();
        let Some(tree) = try_parse(lang, bytes) else {
            eprintln!("skip {lang}: grammar or locals query unavailable in this environment");
            return;
        };
        let bindings = resolve_locals(lang, &tree, bytes).expect("resolve_locals must not error");
        let x_def = src.find("x = 1").expect("x def present");
        let a_def = src.find("(a)").expect("a param present") + "(".len();
        let x_use = src.rfind("x + a").expect("x use present");
        let a_use = src.rfind("x + a").expect("a use present") + "x + ".len();
        assert_eq!(
            bindings.resolved_def(x_use as u32),
            Some(x_def as u32),
            "python: `x` use must bind to local `x = 1`"
        );
        assert_eq!(
            bindings.resolved_def(a_use as u32),
            Some(a_def as u32),
            "python: `a` use must bind to param `a`"
        );
        // The enriched ends are real identifier extents, not the interim zero-width spans.
        let (us, ue, ds, de) = spanned_for(&bindings, x_use as u32).expect("python: x edge spanned");
        assert_eq!(
            ue - us,
            "x".len() as u32,
            "python: x ref end must be the identifier end"
        );
        assert_eq!(
            de - ds,
            "x".len() as u32,
            "python: x def end must be the identifier end"
        );
    }

    #[test]
    fn resolve_locals_binds_go_local() {
        // `x` (block-scoped short var) and param `a` used in the return must bind to the local
        // defs across the nested function/block scopes.
        let lang = "go";
        let src = "package main\n\nfunc outer(a int) int {\n\tx := 1\n\treturn x + a\n}\n";
        let bytes = src.as_bytes();
        let Some(tree) = try_parse(lang, bytes) else {
            eprintln!("skip {lang}: grammar or locals query unavailable in this environment");
            return;
        };
        let bindings = resolve_locals(lang, &tree, bytes).expect("resolve_locals must not error");
        let x_def = src.find("x :=").expect("x def present");
        let a_def = src.find("a int").expect("a param present");
        let x_use = src.rfind("x + a").expect("x use present");
        let a_use = src.rfind("x + a").expect("a use present") + "x + ".len();
        assert_eq!(
            bindings.resolved_def(x_use as u32),
            Some(x_def as u32),
            "go: `x` use must bind to local `x := 1`"
        );
        assert_eq!(
            bindings.resolved_def(a_use as u32),
            Some(a_def as u32),
            "go: `a` use must bind to param `a`"
        );
        let (us, ue, ds, de) = spanned_for(&bindings, x_use as u32).expect("go: x edge spanned");
        assert_eq!(ue - us, "x".len() as u32, "go: x ref end must be the identifier end");
        assert_eq!(de - ds, "x".len() as u32, "go: x def end must be the identifier end");
    }

    #[test]
    fn resolve_locals_binds_typescript_local() {
        let lang = "typescript";
        let src = "function outer(a: number): number {\n  const x = 1;\n  return x + a;\n}\n";
        let bytes = src.as_bytes();
        let Some(tree) = try_parse(lang, bytes) else {
            eprintln!("skip {lang}: grammar or locals query unavailable in this environment");
            return;
        };
        let bindings = resolve_locals(lang, &tree, bytes).expect("resolve_locals must not error");
        let x_def = src.find("x = 1").expect("x def present");
        let a_def = src.find("a: number").expect("a param present");
        let x_use = src.rfind("x + a").expect("x use present");
        let a_use = src.rfind("x + a").expect("a use present") + "x + ".len();
        assert_eq!(
            bindings.resolved_def(x_use as u32),
            Some(x_def as u32),
            "typescript: `x` use must bind to local `const x`"
        );
        assert_eq!(
            bindings.resolved_def(a_use as u32),
            Some(a_def as u32),
            "typescript: `a` use must bind to param `a`"
        );
        let (us, ue, ds, de) = spanned_for(&bindings, x_use as u32).expect("typescript: x edge spanned");
        assert_eq!(
            ue - us,
            "x".len() as u32,
            "typescript: x ref end must be the identifier end"
        );
        assert_eq!(
            de - ds,
            "x".len() as u32,
            "typescript: x def end must be the identifier end"
        );
    }

    #[test]
    fn resolve_locals_binds_tsx_local() {
        let lang = "tsx";
        let src = "function outer(a: number): number {\n  const x = 1;\n  return x + a;\n}\n";
        let bytes = src.as_bytes();
        let Some(tree) = try_parse(lang, bytes) else {
            eprintln!("skip {lang}: grammar or locals query unavailable in this environment");
            return;
        };
        let bindings = resolve_locals(lang, &tree, bytes).expect("resolve_locals must not error");
        let x_def = src.find("x = 1").expect("x def present");
        let a_def = src.find("a: number").expect("a param present");
        let x_use = src.rfind("x + a").expect("x use present");
        let a_use = src.rfind("x + a").expect("a use present") + "x + ".len();
        assert_eq!(
            bindings.resolved_def(x_use as u32),
            Some(x_def as u32),
            "tsx: `x` use must bind to local `const x`"
        );
        assert_eq!(
            bindings.resolved_def(a_use as u32),
            Some(a_def as u32),
            "tsx: `a` use must bind to param `a`"
        );
        let (us, ue, ds, de) = spanned_for(&bindings, x_use as u32).expect("tsx: x edge spanned");
        assert_eq!(ue - us, "x".len() as u32, "tsx: x ref end must be the identifier end");
        assert_eq!(de - ds, "x".len() as u32, "tsx: x def end must be the identifier end");
    }

    #[test]
    fn resolve_locals_binds_real_javascript() {
        // Grammar-backed smoke covering build_bindings' capture-name matching against a real
        // locals.scm. Skips gracefully if the grammar (or its locals query) can't be loaded in
        // this environment — the TSLP grammar download can be unavailable in a cold sandbox/CI.
        use crate::lang::{self, ParseOutcome};
        let lang = "javascript";
        let Ok(Some(_)) = locals_query(lang) else {
            return; // grammar or locals query unavailable — skip, don't fail
        };
        let src = "function outer(a) {\n  let x = 1;\n  return function () { return x + a; };\n}\n";
        let bytes = src.as_bytes();
        let tree = match lang::with_parser(lang, |p| lang::parse_with_default_timeout(p, bytes)) {
            Ok(ParseOutcome::Ok(t)) => t,
            _ => return, // grammar unavailable / parse failed — skip
        };
        let bindings = resolve_locals(lang, &tree, bytes).expect("resolve_locals must not error");

        let x_def = src.find("let x").unwrap() + "let ".len();
        let a_def = src.find("(a)").unwrap() + "(".len();
        let x_use = src.rfind("x + a").unwrap();
        let a_use = src.rfind("x + a").unwrap() + "x + ".len();
        assert_eq!(
            bindings.resolved_def(x_use as u32),
            Some(x_def as u32),
            "inner `x` use must bind to outer `let x`"
        );
        assert_eq!(
            bindings.resolved_def(a_use as u32),
            Some(a_def as u32),
            "inner `a` use must bind to param `a`"
        );
        let (us, ue, ds, de) = spanned_for(&bindings, x_use as u32).expect("javascript: x edge spanned");
        assert_eq!(
            ue - us,
            "x".len() as u32,
            "javascript: x ref end must be the identifier end"
        );
        assert_eq!(
            de - ds,
            "x".len() as u32,
            "javascript: x def end must be the identifier end"
        );
    }
}
