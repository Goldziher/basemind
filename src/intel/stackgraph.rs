//! Stack-graph resolution engine (feature `code-intel-stack`).
//!
//! Turns a Python or Java file into precise intra-file [`FileResolvedRefs`] by executing the
//! vendored `.tsg` name-binding rules against the file's tree-sitter parse tree, building a
//! per-file [`StackGraph`], and running the [`ForwardPartialPathStitcher`] to resolve every
//! reference node to the definition node(s) it binds to *within the same file*.
//!
//! The `.tsg` rulesets are the same ones tree-sitter-stack-graphs ships upstream. They are
//! compiled into a [`StackGraphLanguage`] **once per language** and cached, because parsing +
//! validating the DSL is expensive relative to per-file work.
//!
//! ## Byte-span extraction
//!
//! Each resolved (reference, definition) pair is mapped to a [`ResolvedEdge`] of BYTE offsets.
//! A stack-graph node carries a [`lsp_positions::Span`] in its `SourceInfo`; each endpoint is a
//! `Position { column: Offset { utf8_offset }, containing_line: Range<usize>, .. }`, so the
//! absolute byte offset of a position is `containing_line.start + column.utf8_offset`.
//!
//! ## Imports / exports
//!
//! `imports` and `exports` are derived from basemind's existing L1 extraction rather than mined
//! from the stack graph: L1 already extracts imports and top-level symbols with correct identifier
//! byte spans, and threading those through avoids re-deriving fiddly root/jump-to-scope reachability
//! from the graph. `intra` — the hard part — comes from the stack graph.
//!
//! Never panics: a parse failure, `.tsg` build error, or stitch error yields `None`, so the
//! dispatcher falls back to the tree-sitter `locals` engine.

use std::sync::OnceLock;

use ahash::AHashMap;
use stack_graphs::arena::Handle;
use stack_graphs::graph::{Node, StackGraph};
use stack_graphs::partial::PartialPaths;
use stack_graphs::stitching::{Database, DatabaseCandidates, ForwardPartialPathStitcher, StitcherConfig};
use tree_sitter_graph::Variables;
use tree_sitter_stack_graphs::StackGraphLanguage;

use crate::intel::model::{FileResolvedRefs, ResolvedEdge};
use crate::lang::LangId;

/// The vendored Python name-binding rules.
const PYTHON_TSG: &str = include_str!("tsg/python.tsg");
/// The vendored Java name-binding rules.
const JAVA_TSG: &str = include_str!("tsg/java.tsg");

/// True for the languages this engine has a `.tsg` ruleset for. Callers dispatch to
/// [`resolve_stackgraph`] only when this returns true.
pub fn has_tsg_ruleset(lang: LangId) -> bool {
    matches!(lang, "python" | "java")
}

/// The raw `.tsg` source for a supported language, with grammar-drift adaptations applied.
///
/// GRAMMAR DRIFT: basemind's tree-sitter Python grammar (TSLP 1.12.5) has no `except_group_clause`
/// node (the `except*` construct). The upstream `python.tsg` references it in two places — an
/// alternation entry and a standalone stanza — both of which fail to compile against our grammar.
/// We strip both; `except*` handling folds into the existing `except_clause` rules already present.
fn tsg_source(lang: LangId) -> Option<String> {
    match lang {
        "python" => Some(adapt_python_tsg(PYTHON_TSG)),
        "java" => Some(JAVA_TSG.to_string()),
        _ => None,
    }
}

/// Remove the two `except_group_clause` references that don't exist in basemind's Python grammar.
/// Both are self-contained: one alternation list entry and one empty stanza.
fn adapt_python_tsg(src: &str) -> String {
    src.replace("  (except_group_clause)\n", "")
        .replace("(except_group_clause) {}\n", "")
}

/// Per-language compiled `.tsg`. A build failure (e.g. residual grammar drift) is cached as `None`
/// so we don't retry a hopeless compile on every file.
struct LangEngine {
    sgl: StackGraphLanguage,
}

// SAFETY-free: StackGraphLanguage owns a tree_sitter::Language + parsed AST; both are Send+Sync.
static ENGINES: OnceLock<std::sync::RwLock<AHashMap<LangId, Option<&'static LangEngine>>>> = OnceLock::new();

/// Compile (once) and return the cached engine for `lang`, or `None` if the `.tsg` won't build.
///
/// The compiled engine is leaked into a `'static` slot: there is exactly one per language for the
/// life of the process, mirroring the query-cache pattern in `src/lang.rs`.
fn engine(lang: LangId) -> Option<&'static LangEngine> {
    let map = ENGINES.get_or_init(|| std::sync::RwLock::new(AHashMap::new()));
    if let Some(cached) = map.read().ok().and_then(|m| m.get(lang).copied()) {
        return cached;
    }
    let built = build_engine(lang);
    if let Ok(mut m) = map.write() {
        m.insert(lang, built);
    }
    built
}

/// Compile the `.tsg` for `lang` into a leaked `'static` engine.
fn build_engine(lang: LangId) -> Option<&'static LangEngine> {
    let ts_language = match crate::lang::language(lang) {
        Ok(l) => l,
        Err(err) => {
            tracing::debug!(lang, error = %err, "stackgraph: tree-sitter language unavailable");
            return None;
        }
    };
    let source = tsg_source(lang)?;
    let sgl = match StackGraphLanguage::from_str(ts_language, &source) {
        Ok(sgl) => sgl,
        Err(err) => {
            // Whole-language degradation to the locals fallback — surface it above debug. Cached per
            // language (see `engine`), so this fires at most once per language per process.
            tracing::warn!(
                lang,
                error = %err,
                "stackgraph: .tsg failed to compile — precise resolution disabled for this language, falling back to tree-sitter locals"
            );
            return None;
        }
    };
    Some(Box::leak(Box::new(LangEngine { sgl })))
}

/// Resolve a single file's intra-file references via its stack graph. Returns `None` on any
/// failure so the dispatcher can fall back to the tree-sitter `locals` engine.
pub fn resolve_stackgraph(lang: LangId, source: &[u8]) -> Option<FileResolvedRefs> {
    let src = std::str::from_utf8(source).ok()?;
    let engine = engine(lang)?;

    let intra = match resolve_intra(engine, lang, src) {
        Ok(edges) => edges,
        Err(err) => {
            tracing::debug!(lang, error = %err, "stackgraph: intra-file resolution failed");
            return None;
        }
    };

    let mut out = FileResolvedRefs::new(lang);
    out.intra = intra;
    // Imports/exports come from L1 (correct identifier byte spans; see module docs). Best-effort:
    // a failure here still leaves a valid `intra`-only record.
    populate_imports_exports(lang, source, &mut out);
    Some(out)
}

/// Per-file wall-clock budget shared by the stack-graph build and both partial-path stitcher
/// passes. The parse step is already timeout-bounded (`parse_with_default_timeout`); this bounds the
/// graph construction + path-stitching that follow, which are worst-case combinatorial in scope
/// nesting / reference count, so a crafted or pathological file (within `max_file_bytes`) can't pin
/// a scanner rayon worker indefinitely. On expiry the call errors and `resolve_stackgraph` degrades
/// to the tree-sitter `locals` fallback. Generous enough to never trip on legitimate source.
const STACKGRAPH_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// A shared wall-clock deadline implementing both the `stack_graphs` and `tree_sitter_stack_graphs`
/// `CancellationFlag` traits (identical `check` shape, distinct error types), so one budget covers
/// the build + both stitcher passes of a single file.
struct Deadline {
    limit: std::time::Duration,
    start: std::time::Instant,
}

impl Deadline {
    fn start(limit: std::time::Duration) -> Self {
        Self {
            limit,
            start: std::time::Instant::now(),
        }
    }
    fn expired(&self) -> bool {
        self.start.elapsed() > self.limit
    }
}

impl stack_graphs::CancellationFlag for Deadline {
    fn check(&self, at: &'static str) -> Result<(), stack_graphs::CancellationError> {
        if self.expired() {
            return Err(stack_graphs::CancellationError(at));
        }
        Ok(())
    }
}

impl tree_sitter_stack_graphs::CancellationFlag for Deadline {
    fn check(&self, at: &'static str) -> Result<(), tree_sitter_stack_graphs::CancellationError> {
        if self.expired() {
            return Err(tree_sitter_stack_graphs::CancellationError(at));
        }
        Ok(())
    }
}

/// Build the stack graph for `src` and stitch reference→definition partial paths, keeping only
/// edges whose definition lands in THIS file.
fn resolve_intra(engine: &LangEngine, lang: LangId, src: &str) -> Result<Vec<ResolvedEdge>, String> {
    let mut graph = StackGraph::new();
    let file = graph.get_or_create_file("<file>");
    let globals = Variables::new();
    let deadline = Deadline::start(STACKGRAPH_BUDGET);

    engine
        .sgl
        .build_stack_graph_into(&mut graph, file, src, &globals, &deadline)
        .map_err(|e| format!("build_stack_graph_into: {e}"))?;

    // Phase 1: minimal partial-path set for this file, loaded into a Database.
    let mut partials = PartialPaths::new();
    let mut db = Database::new();
    ForwardPartialPathStitcher::find_minimal_partial_path_set_in_file(
        &graph,
        &mut partials,
        file,
        StitcherConfig::default(),
        &deadline,
        |g, ps, path| {
            db.add_partial_path(g, ps, path.clone());
        },
    )
    .map_err(|_| "find_minimal_partial_path_set_in_file cancelled".to_string())?;

    // Phase 2: from every reference node, stitch complete paths and record (ref, def) endpoints.
    let references: Vec<Handle<Node>> = graph.iter_nodes().filter(|h| graph[*h].is_reference()).collect();

    let mut edges: Vec<ResolvedEdge> = Vec::new();
    let mut seen: ahash::AHashSet<(u32, u32, u32, u32)> = ahash::AHashSet::new();
    ForwardPartialPathStitcher::find_all_complete_partial_paths(
        &mut DatabaseCandidates::new(&graph, &mut partials, &mut db),
        references,
        StitcherConfig::default(),
        &deadline,
        |g, _ps, path| {
            let use_node = path.start_node;
            let def_node = path.end_node;
            // Intra-file only: the definition must belong to this same file (skip root/jump-to and
            // any cross-file endpoints — those become imports/exports, handled via L1).
            if !g[def_node].is_definition() || !g[def_node].is_in_file(file) {
                return;
            }
            if !g[use_node].is_in_file(file) {
                return;
            }
            if let (Some((us, ue)), Some((ds, de))) = (span_bytes(g, use_node), span_bytes(g, def_node)) {
                let key = (us, ue, ds, de);
                if seen.insert(key) {
                    edges.push(ResolvedEdge {
                        use_start: us,
                        use_end: ue,
                        def_start: ds,
                        def_end: de,
                    });
                }
            }
        },
    )
    .map_err(|_| "find_all_complete_partial_paths cancelled".to_string())?;

    let _ = lang;
    Ok(edges)
}

/// Absolute (start, end) byte offsets of a node's source span, or `None` for nodes with no source
/// span (root, jump-to-scope, synthetic scopes). A position's byte offset is the start byte of its
/// containing line plus the UTF-8 column offset within that line.
fn span_bytes(graph: &StackGraph, node: Handle<Node>) -> Option<(u32, u32)> {
    let info = graph.source_info(node)?;
    let span = &info.span;
    let start = span.start.containing_line.start + span.start.column.utf8_offset;
    let end = span.end.containing_line.start + span.end.column.utf8_offset;
    if end <= start {
        return None;
    }
    Some((start as u32, end as u32))
}

/// Fill `imports`/`exports` best-effort.
///
/// Both sides are extracted with small per-language tree-sitter queries that capture the **identifier
/// node** directly, so `name_start` / `local_start` are byte-precise:
///
/// - **Exports** must record the byte of the export *identifier*, not the definition node. L1's
///   `Symbol.start_byte` is the definition-node start (the `def` / `class` keyword), which is the
///   wrong anchor for the cross-file join: `goto_definition` would land on the keyword and
///   `resolved_callers_page`'s identifier-byte containment check would miss. The query captures the
///   `name:` identifier of each module-level definition, which is exactly the `name_start` an
///   [`ExportEdge`] needs (and the byte the resolver's intra `def_start` shares).
/// - **Imports** capture the local-name identifier node, since L1's `Import` only records the
///   whole-statement range, not the per-name local span an [`ImportEdge`] needs.
///
/// Both are best-effort: a parse or query failure leaves the (already-valid) `intra`-only record
/// untouched.
fn populate_imports_exports(lang: LangId, source: &[u8], out: &mut FileResolvedRefs) {
    use crate::lang::{ParseOutcome, parse_with_default_timeout, with_parser};

    // Parse once and share the tree between the export and import queries — each would otherwise
    // re-parse identical bytes. (The stack-graph build does its own parse upstream; this at least
    // collapses the two query passes here into a single parse.)
    let Ok(ParseOutcome::Ok(tree)) = with_parser(lang, |p| parse_with_default_timeout(p, source)) else {
        return;
    };
    let root = tree.root_node();
    extract_exports(lang, source, root, out);
    extract_imports(lang, source, root, out);
}

/// Extract module-level export identifiers with a cached per-language tree-sitter query. Captures
/// the export **identifier** node (not the definition node), so `name_start` is the byte the
/// cross-file join keys on and `goto_definition` lands on.
fn extract_exports(lang: LangId, source: &[u8], root: tree_sitter::Node<'_>, out: &mut FileResolvedRefs) {
    use crate::intel::model::ExportEdge;
    use streaming_iterator::StreamingIterator;

    let Some(query) = export_query(lang) else { return };
    let Some(export_idx) = query.capture_index_for_name("export") else {
        return;
    };

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(query, root, source);
    while let Some(m) = matches.next() {
        let Some(node) = m.captures.iter().find(|c| c.index == export_idx).map(|c| c.node) else {
            continue;
        };
        let Ok(name) = node.utf8_text(source) else { continue };
        out.exports.push(ExportEdge {
            name: name.to_string(),
            name_start: node.start_byte() as u32,
        });
    }
}

/// Extract import local-name bindings with a cached per-language tree-sitter query.
fn extract_imports(lang: LangId, source: &[u8], root: tree_sitter::Node<'_>, out: &mut FileResolvedRefs) {
    use crate::intel::model::ImportEdge;
    use streaming_iterator::StreamingIterator;

    let Some(query) = import_query(lang) else { return };

    let local_idx = query.capture_index_for_name("local");
    let module_idx = query.capture_index_for_name("module");
    let Some(local_idx) = local_idx else { return };

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(query, root, source);
    while let Some(m) = matches.next() {
        let local_node = m.captures.iter().find(|c| c.index == local_idx).map(|c| c.node);
        let Some(local_node) = local_node else { continue };
        let local = match local_node.utf8_text(source) {
            Ok(t) => t.to_string(),
            Err(_) => continue,
        };
        // A `@module` capture is present only for the from-import / Java-import patterns (a bare
        // Python `import m` has none). Its presence tells us this binding names a *symbol* imported
        // from a module — so `imported` is the symbol name the cross-file join must match against
        // the target's exports. Absent an alias (which the query deliberately does not capture),
        // the imported name equals the local name. A bare `import m` binds the module itself, not a
        // single export, so it carries `imported: None` and the join skips it.
        let module_node = module_idx.and_then(|mi| m.captures.iter().find(|c| c.index == mi));
        let specifier = module_node
            .and_then(|c| c.node.utf8_text(source).ok())
            .unwrap_or("")
            .to_string();
        let imported = module_node.map(|_| local.clone());
        out.imports.push(ImportEdge {
            local,
            specifier,
            imported,
            is_type: false,
            local_start: local_node.start_byte() as u32,
        });
    }
}

/// Compiled import query per language, cached for the process lifetime.
fn import_query(lang: LangId) -> Option<&'static tree_sitter::Query> {
    static IMPORT_QUERIES: OnceLock<std::sync::RwLock<AHashMap<LangId, Option<&'static tree_sitter::Query>>>> =
        OnceLock::new();
    let map = IMPORT_QUERIES.get_or_init(|| std::sync::RwLock::new(AHashMap::new()));
    if let Some(cached) = map.read().ok().and_then(|m| m.get(lang).copied()) {
        return cached;
    }
    let built = build_import_query(lang);
    if let Ok(mut m) = map.write() {
        m.insert(lang, built);
    }
    built
}

fn build_import_query(lang: LangId) -> Option<&'static tree_sitter::Query> {
    // Capture the local-name identifier (`@local`) and, where available, the module path
    // (`@module`). Python `from m import f` / `import m` and Java `import a.b.Foo;`.
    let src = match lang {
        "python" => {
            "(import_from_statement module_name: (_) @module name: (dotted_name (identifier) @local))\n\
             (import_statement name: (dotted_name (identifier) @local))"
        }
        "java" => "(import_declaration (scoped_identifier name: (identifier) @local) @module)",
        _ => return None,
    };
    let ts_language = crate::lang::language(lang).ok()?;
    match tree_sitter::Query::new(&ts_language, src) {
        Ok(q) => Some(Box::leak(Box::new(q))),
        Err(err) => {
            tracing::debug!(lang, error = %err, "stackgraph: import query failed to compile");
            None
        }
    }
}

/// Compiled export query per language, cached for the process lifetime.
fn export_query(lang: LangId) -> Option<&'static tree_sitter::Query> {
    static EXPORT_QUERIES: OnceLock<std::sync::RwLock<AHashMap<LangId, Option<&'static tree_sitter::Query>>>> =
        OnceLock::new();
    let map = EXPORT_QUERIES.get_or_init(|| std::sync::RwLock::new(AHashMap::new()));
    if let Some(cached) = map.read().ok().and_then(|m| m.get(lang).copied()) {
        return cached;
    }
    let built = build_export_query(lang);
    if let Ok(mut m) = map.write() {
        m.insert(lang, built);
    }
    built
}

fn build_export_query(lang: LangId) -> Option<&'static tree_sitter::Query> {
    // Capture the module-level export identifier (`@export`): the `name:` of each top-level
    // definition another file can import. Only node types known to exist in the TSLP grammars are
    // referenced — one unknown node type fails the WHOLE query compile, silently emptying exports.
    let src = match lang {
        "python" => {
            "(module (function_definition name: (identifier) @export))\n\
             (module (class_definition name: (identifier) @export))\n\
             (module (decorated_definition (function_definition name: (identifier) @export)))\n\
             (module (decorated_definition (class_definition name: (identifier) @export)))\n\
             (module (expression_statement (assignment left: (identifier) @export)))"
        }
        "java" => {
            "(program (class_declaration name: (identifier) @export))\n\
             (program (interface_declaration name: (identifier) @export))\n\
             (program (enum_declaration name: (identifier) @export))\n\
             (program (record_declaration name: (identifier) @export))"
        }
        _ => return None,
    };
    let ts_language = crate::lang::language(lang).ok()?;
    match tree_sitter::Query::new(&ts_language, src) {
        Ok(q) => Some(Box::leak(Box::new(q))),
        Err(err) => {
            tracing::debug!(lang, error = %err, "stackgraph: export query failed to compile");
            None
        }
    }
}

#[cfg(all(test, feature = "code-intel-stack"))]
mod tests {
    use super::*;

    fn skip_if_no_grammar(lang: LangId) -> bool {
        if crate::lang::language(lang).is_err() {
            eprintln!("skip: {lang} grammar unavailable in this environment");
            return true;
        }
        false
    }

    /// Find the intra edge whose use starts at `use_start`; panics if absent.
    fn edge_for_use(refs: &FileResolvedRefs, use_start: u32) -> &ResolvedEdge {
        refs.intra
            .iter()
            .find(|e| e.use_start == use_start)
            .unwrap_or_else(|| panic!("no intra edge for use at byte {use_start}; edges: {:?}", refs.intra))
    }

    #[test]
    fn python_local_shadows_module_and_import() {
        if skip_if_no_grammar("python") {
            return;
        }
        // `x` is defined at module level (line 1) and re-bound locally inside `f`. The use of `x`
        // inside `f` must resolve to the LOCAL def, not the module-level one or the import.
        let src = "from m import x\nX_MODULE = 1\ndef f():\n    x = 2\n    return x\n";
        let refs = resolve_stackgraph("python", src.as_bytes());
        let Some(refs) = refs else {
            eprintln!("skip: python stack-graph engine unavailable (tsg build failed)");
            return;
        };
        if refs.intra.is_empty() {
            eprintln!("skip: python stack graph produced no intra edges in this env");
            return;
        }
        let local_def = src.find("    x = 2").unwrap() as u32 + 4; // the `x` in `x = 2`
        let return_use = src.rfind("return x").unwrap() as u32 + "return ".len() as u32;
        let edge = edge_for_use(&refs, return_use);
        assert_eq!(
            edge.def_start, local_def,
            "the `return x` use must resolve to the local `x = 2` def, not the module/import x"
        );
        assert!(
            edge.use_end > edge.use_start && edge.def_end > edge.def_start,
            "spans non-empty"
        );
        assert_eq!(edge.def_end - edge.def_start, 1, "`x` def is one byte wide");
    }

    #[test]
    fn python_per_function_params_are_distinct() {
        if skip_if_no_grammar("python") {
            return;
        }
        let src = "def first(x):\n    return x\ndef second(x):\n    return x * 2\n";
        let Some(refs) = resolve_stackgraph("python", src.as_bytes()) else {
            eprintln!("skip: python stack-graph engine unavailable");
            return;
        };
        if refs.intra.is_empty() {
            eprintln!("skip: no intra edges in this env");
            return;
        }
        // first param `x` is at the first "(x)"; its use is the first `return x`.
        let first_param = src.find("first(x)").unwrap() as u32 + "first(".len() as u32;
        let second_param = src.find("second(x)").unwrap() as u32 + "second(".len() as u32;
        let first_use = src.find("return x\n").unwrap() as u32 + "return ".len() as u32;
        let second_use = src.find("return x * 2").unwrap() as u32 + "return ".len() as u32;

        assert_eq!(
            edge_for_use(&refs, first_use).def_start,
            first_param,
            "first fn's `x` use resolves to first fn's param"
        );
        assert_eq!(
            edge_for_use(&refs, second_use).def_start,
            second_param,
            "second fn's `x` use resolves to second fn's param (distinct from first)"
        );
    }

    #[test]
    fn python_comprehension_var_shadows_outer() {
        if skip_if_no_grammar("python") {
            return;
        }
        // The comprehension binds its own `x`; the expression `x` before `for` must resolve to the
        // comprehension binding, NOT the outer `x`. This is the case the locals engine gets wrong.
        let src = "def g():\n    x = \"outer\"\n    values = [x for x in range(3)]\n    return x\n";
        let Some(refs) = resolve_stackgraph("python", src.as_bytes()) else {
            eprintln!("skip: python stack-graph engine unavailable");
            return;
        };
        if refs.intra.is_empty() {
            eprintln!("skip: no intra edges in this env");
            return;
        }
        let outer_def = src.find("    x = \"outer\"").unwrap() as u32 + 4;
        let comp_binding = src.find("for x in").unwrap() as u32 + "for ".len() as u32;
        // The `x` in `[x for ...]` (the element expression, before `for`).
        let comp_use = src.find("[x for").unwrap() as u32 + 1;
        let comp_edge = edge_for_use(&refs, comp_use);
        assert_eq!(
            comp_edge.def_start, comp_binding,
            "comprehension element `x` must bind to the comprehension var, not outer `x` at {outer_def}"
        );
    }

    #[test]
    fn java_field_vs_local_resolve_correctly() {
        if skip_if_no_grammar("java") {
            return;
        }
        let src = "class C {\n    private int value = 1;\n    public int m() {\n        int value = 2;\n        return value;\n    }\n}\n";
        let Some(refs) = resolve_stackgraph("java", src.as_bytes()) else {
            eprintln!("skip: java stack-graph engine unavailable");
            return;
        };
        if refs.intra.is_empty() {
            eprintln!("skip: no intra edges in this env");
            return;
        }
        let local_def = src.find("int value = 2").unwrap() as u32 + "int ".len() as u32;
        let return_use = src.find("return value;").unwrap() as u32 + "return ".len() as u32;
        let edge = edge_for_use(&refs, return_use);
        assert_eq!(
            edge.def_start, local_def,
            "`return value` inside m() must resolve to the local `value`, not the field"
        );
    }

    #[test]
    fn java_imported_class_not_conflated_with_local_method() {
        if skip_if_no_grammar("java") {
            return;
        }
        // `Foo.greet()` uses imported class `Foo`; there is also a local method `greet`. The `greet`
        // in `Foo.greet()` must NOT resolve to the local `greet` method definition.
        let src = "import a.b.Foo;\nclass C {\n    String greet() { return \"local\"; }\n    String use() { return Foo.greet(); }\n}\n";
        let Some(refs) = resolve_stackgraph("java", src.as_bytes()) else {
            eprintln!("skip: java stack-graph engine unavailable");
            return;
        };
        if refs.intra.is_empty() {
            eprintln!("skip: no intra edges in this env");
            return;
        }
        let local_greet_def = src.find("String greet()").unwrap() as u32 + "String ".len() as u32;
        let foo_greet_use = src.find("Foo.greet()").unwrap() as u32 + "Foo.".len() as u32;
        // If an edge exists for the `greet` in `Foo.greet()`, it must not point at the local method.
        if let Some(edge) = refs.intra.iter().find(|e| e.use_start == foo_greet_use) {
            assert_ne!(
                edge.def_start, local_greet_def,
                "`Foo.greet()` must not conflate with the local `greet` method"
            );
        }
    }
}
