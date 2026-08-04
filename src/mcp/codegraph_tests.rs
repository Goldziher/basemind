//! Unit tests for the shared codegraph model (extracted from `codegraph.rs` to keep it
//! under the 1000-line module cap). Included via `#[path]` from `codegraph.rs`.

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
    assert_eq!(EdgeKind::Annotates.as_str(), "annotates");
    assert_eq!(EdgeKind::Cites.as_str(), "cites");
}

#[test]
fn decision_ids_normalize_consistently() {
    use crate::path::RelPath;
    // The id extracted from a path must equal the canonical citation id, so a rationale note
    // citing "ADR-0007" resolves to `docs/adr/0007-*.md`.
    assert_eq!(normalize_decision_id("adr", 7), "ADR-0007");
    assert_eq!(normalize_decision_id("RFC", 2119), "RFC-2119");
    assert_eq!(
        decision_id_of_path(&RelPath::from("docs/adr/0007-graph.md")).as_deref(),
        Some("ADR-0007")
    );
    assert_eq!(
        decision_id_of_path(&RelPath::from("spec/rfc/2119-keywords.txt")).as_deref(),
        Some("RFC-2119")
    );
    // An ordinary source file is not a decision record.
    assert_eq!(decision_id_of_path(&RelPath::from("src/mcp/codegraph.rs")), None);
    // Digits-in-name but not under an adr/rfc directory is not a decision record.
    assert_eq!(decision_id_of_path(&RelPath::from("src/0001-notes.md")), None);
}

#[test]
fn attach_symbol_prefers_containing_then_following() {
    // syms: A[0,10)  B[20,40) with inner note at 25, doc-comment note at 15 (between A and B).
    let syms = [(0u32, 10u32), (20u32, 40u32)];
    // Inside B's span → attaches to B.
    assert_eq!(attach_symbol(&syms, 25), Some(20));
    // Inside A's span → attaches to A.
    assert_eq!(attach_symbol(&syms, 5), Some(0));
    // Between A and B (a doc-comment above B) → attaches to the following symbol B.
    assert_eq!(attach_symbol(&syms, 15), Some(20));
    // After the last symbol → no symbol near, attaches to the file.
    assert_eq!(attach_symbol(&syms, 50), None);
}

/// Build a synthetic single-file cache carrying rationale records, to exercise the rationale
/// lane directly (the extractor that populates `l1.rationale` lands in the ADR-0009 producer).
fn cache_with_rationale(
    files: &[(&str, Vec<crate::extract::Symbol>, Vec<crate::extract::RationaleRecord>)],
) -> MapCache {
    use crate::extract::FileMapL1;
    use crate::path::RelPath;
    let mut cache = MapCache::empty();
    for (path, symbols, rationale) in files {
        let l1 = FileMapL1 {
            schema_ver: crate::extract::SCHEMA_VER,
            language: "rust".to_string(),
            size_bytes: 0,
            had_errors: false,
            error_count: 0,
            symbols: symbols.clone(),
            imports: Vec::new(),
            implementations: Vec::new(),
            rationale: rationale.clone(),
        };
        cache.by_path.insert(RelPath::from(*path), l1);
    }
    cache
}

fn sym(name: &str, start: u32, end: u32) -> crate::extract::Symbol {
    crate::extract::Symbol {
        name: name.to_string(),
        kind: crate::extract::SymbolKind::Function,
        start_byte: start,
        end_byte: end,
        start_row: 0,
        start_col: 0,
        signature: None,
        decorators: Vec::new(),
    }
}

fn rationale(kind: crate::extract::RationaleKind, start: u32, citations: &[&str]) -> crate::extract::RationaleRecord {
    crate::extract::RationaleRecord {
        kind,
        text: "why we did it".to_string(),
        start_byte: start,
        citations: citations.iter().map(|c| c.to_string()).collect(),
    }
}

fn build_from(cache: &MapCache, kinds: EdgeKindSet) -> CodeGraph {
    build(
        None,
        cache,
        &BuildOpts {
            kinds,
            focus: None,
            scan_cap: CODEGRAPH_SCAN_CAP,
        },
    )
    .expect("build")
}

#[test]
fn rationale_annotates_the_enclosing_symbol() {
    use crate::extract::RationaleKind;
    // A note inside `helper`'s span [0,20) → an Annotates edge from the Rationale node to helper.
    let cache = cache_with_rationale(&[(
        "m.rs",
        vec![sym("helper", 0, 20)],
        vec![rationale(RationaleKind::Why, 5, &[])],
    )]);
    let g = build_from(&cache, EdgeKindSet::all());
    let ann: Vec<&CodeEdge> = g.edges.iter().filter(|e| e.kind == EdgeKind::Annotates).collect();
    assert_eq!(ann.len(), 1, "one annotates edge");
    assert_eq!(
        ann[0].provenance,
        Provenance::Inferred,
        "proximity attachment is inferred"
    );
    assert!(matches!(&ann[0].from, NodeKey::Rationale { start_byte: 5, .. }));
    assert!(
        matches!(&ann[0].to, NodeKey::Symbol { start_byte: 0, .. }),
        "attaches to the enclosing symbol"
    );
}

#[test]
fn rationale_cites_resolve_to_decision_or_virtual_name() {
    use crate::extract::RationaleKind;
    // `m.rs` cites ADR-0001 (present as a decision file) and ADR-0099 (absent).
    let cache = cache_with_rationale(&[
        (
            "m.rs",
            vec![sym("helper", 0, 20)],
            vec![rationale(RationaleKind::Why, 5, &["ADR-0001", "ADR-0099"])],
        ),
        ("docs/adr/0001-codegraph.md", vec![], vec![]),
    ]);
    let g = build_from(&cache, EdgeKindSet::all());
    let cites: Vec<&CodeEdge> = g.edges.iter().filter(|e| e.kind == EdgeKind::Cites).collect();
    assert_eq!(cites.len(), 2, "one cite per citation");
    let resolved = cites
        .iter()
        .find(|e| matches!(&e.to, NodeKey::Decision { .. }))
        .expect("ADR-0001 resolves to a decision node");
    assert_eq!(
        resolved.provenance,
        Provenance::Extracted,
        "a resolved citation is extracted"
    );
    let dangling = cites
        .iter()
        .find(|e| matches!(&e.to, NodeKey::Name(n) if n == "ADR-0099"))
        .expect("ADR-0099 falls back to a virtual name node");
    assert_eq!(
        dangling.provenance,
        Provenance::Inferred,
        "an unresolved citation is inferred"
    );
}

#[test]
fn rationale_lanes_are_opt_in() {
    use crate::extract::RationaleKind;
    let cache = cache_with_rationale(&[(
        "m.rs",
        vec![sym("helper", 0, 20)],
        vec![rationale(RationaleKind::Why, 5, &["ADR-0001"])],
    )]);
    // calls-only build must not emit any rationale edges.
    let calls_only = EdgeKindSet {
        calls: true,
        ..EdgeKindSet::none()
    };
    let g = build_from(&cache, calls_only);
    assert!(
        !g.edges
            .iter()
            .any(|e| matches!(e.kind, EdgeKind::Annotates | EdgeKind::Cites)),
        "rationale lanes stay off unless selected"
    );
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
    // The module-scope `helper()` call is enclosed by no function (it precedes `outer`), so it ~keep
    // falls to the file node; the call inside `outer` attributes to `outer`. Guards the ~keep
    // `enclosing` fallback path the partition-point rewrite must preserve. ~keep
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
