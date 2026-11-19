//! The import graph over synthetic edge rows (adjacency, walks and their cuts, cycles, the
//! type-only tier, coverage, the cache), and through a real scan and rescan of a fixture.

use std::cell::Cell;

use super::*;
use crate::index::keys::{EdgeKinds, FileEdge};

/// A runtime import, a type-only import and a module declaration, for edge rows.
const I: EdgeKinds = EdgeKinds::IMPORT;
const T: EdgeKinds = EdgeKinds::TYPE_ONLY;
const M: EdgeKinds = EdgeKinds::MODULE_DECL;
/// Runtime and type-only edges.
const TYPES: Tiers = Tiers {
    type_imports: true,
    module_decls: false,
};

/// Edge rows from `(importer, [(target, kinds)])`.
fn rows(spec: &[(&str, &[(&str, EdgeKinds)])]) -> Vec<(RelPath, FileEdges)> {
    spec.iter()
        .map(|(importer, targets)| {
            let mut edges: Vec<FileEdge> = targets
                .iter()
                .enumerate()
                .map(|(at, (target, kinds))| FileEdge {
                    target: RelPath::from(*target),
                    local_start: u32::try_from(at).unwrap() * 10,
                    kinds: *kinds,
                })
                .collect();
            edges.sort_unstable_by(|a, b| a.target.cmp(&b.target));
            (
                RelPath::from(*importer),
                FileEdges {
                    edges,
                    ..FileEdges::default()
                },
            )
        })
        .collect()
}

/// A graph of runtime edges `(importer, target)`, every file in language `ts`.
fn graph(edges: &[(&str, &str)]) -> ImportGraph {
    let mut by_importer: Vec<(&str, Vec<(&str, EdgeKinds)>)> = Vec::new();
    let mut slot: AHashMap<&str, usize> = AHashMap::new();
    for &(importer, target) in edges {
        let at = *slot.entry(importer).or_insert_with(|| {
            by_importer.push((importer, Vec::new()));
            by_importer.len() - 1
        });
        by_importer[at].1.push((target, I));
    }
    let spec: Vec<(&str, &[(&str, EdgeKinds)])> = by_importer
        .iter()
        .map(|(importer, targets)| (*importer, targets.as_slice()))
        .collect();
    ImportGraph::build(rows(&spec), std::iter::empty(), |_| true, true)
}

fn id(graph: &ImportGraph, path: &str) -> NodeId {
    graph.node(path).unwrap_or_else(|| panic!("{path} is a node"))
}

fn names(graph: &ImportGraph, nodes: impl IntoIterator<Item = NodeId>) -> Vec<String> {
    nodes.into_iter().map(|node| graph.path(node).to_string()).collect()
}

fn reached(graph: &ImportGraph, walk: &Walk) -> Vec<(String, u8)> {
    walk.reached
        .iter()
        .map(|hit| (graph.path(hit.node).to_string(), hit.depth))
        .collect()
}

fn cycles(graph: &ImportGraph, tiers: Tiers) -> Vec<Vec<String>> {
    graph
        .cycles(tiers)
        .into_iter()
        .map(|cycle| names(graph, cycle))
        .collect()
}

/// `a` imports `b` and `c`, both import `d`.
fn diamond() -> ImportGraph {
    graph(&[("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")])
}

#[test]
fn adjacency_runs_both_ways_in_path_order() {
    let g = diamond();
    assert_eq!((g.len(), g.edge_count()), (4, 4));
    let (a, d) = (id(&g, "a"), id(&g, "d"));
    let deps = g.dependencies(a, Tiers::RUNTIME).map(|edge| edge.node);
    assert_eq!(names(&g, deps), ["b", "c"]);
    let importers = g.importers(d, Tiers::RUNTIME).map(|edge| edge.node);
    assert_eq!(names(&g, importers), ["b", "c"]);
    assert_eq!(g.in_degree(d, Tiers::RUNTIME), 2);
    assert_eq!(g.in_degree(a, Tiers::RUNTIME), 0);
    assert!(g.node("e").is_none() && g.node("").is_none());
    let into_d: Vec<u32> = g.importers(d, Tiers::RUNTIME).map(|edge| edge.local_start).collect();
    assert_eq!(into_d, [0, 0], "the reverse edge keeps the offset in the importer");
}

#[test]
fn a_file_only_imported_is_a_node() {
    let g = graph(&[("src/app.ts", "src/leaf.ts")]);
    let leaf = id(&g, "src/leaf.ts");
    assert_eq!(g.dependencies(leaf, TYPES).count(), 0);
    assert_eq!(g.in_degree(leaf, Tiers::RUNTIME), 1);
}

#[test]
fn the_diamond_walks_by_depth_and_reports_the_depth_cut() {
    let g = diamond();
    let a = id(&g, "a");
    let one = g.walk(&[a], Direction::Dependencies, WalkLimits::new(None), Tiers::RUNTIME);
    assert_eq!(reached(&g, &one), [("b".to_string(), 1), ("c".to_string(), 1)]);
    assert_eq!(one.truncated, Some(Truncation::Depth), "d lies past depth 1");
    assert_eq!(one.truncated.map(Truncation::reason), Some("depth"));

    let two = g.walk(&[a], Direction::Dependencies, WalkLimits::new(Some(2)), Tiers::RUNTIME);
    assert_eq!(
        reached(&g, &two),
        [("b".to_string(), 1), ("c".to_string(), 1), ("d".to_string(), 2)],
        "d is reported once, at its nearest depth"
    );
    assert_eq!(two.truncated, None, "nothing lies past d");
    assert_eq!(two.reached[2].via, id(&g, "b"), "first reached through the first path");

    let up = g.walk(
        &[id(&g, "d")],
        Direction::Importers,
        WalkLimits::new(Some(5)),
        Tiers::RUNTIME,
    );
    assert_eq!(
        reached(&g, &up),
        [("b".to_string(), 1), ("c".to_string(), 1), ("a".to_string(), 2)]
    );
    assert_eq!(up.truncated, None);
}

#[test]
fn walk_limits_hold_the_depth_to_its_range() {
    assert_eq!(WalkLimits::new(None).depth, DEPTH_DEFAULT);
    assert_eq!(WalkLimits::new(Some(0)).depth, 1);
    assert_eq!(WalkLimits::new(Some(9)).depth, DEPTH_MAX);
    assert_eq!(WalkLimits::new(Some(3)).node_cap, NODE_CAP);
}

#[test]
fn a_walk_past_the_node_cap_stops_with_scan_cap() {
    let importers: Vec<String> = (0..10).map(|n| format!("m{n}")).collect();
    let edges: Vec<(&str, &str)> = importers.iter().map(|m| (m.as_str(), "hub")).collect();
    let g = graph(&edges);
    let hub = id(&g, "hub");
    let limits = WalkLimits { depth: 1, node_cap: 3 };
    let walk = g.walk(&[hub], Direction::Importers, limits, Tiers::RUNTIME);
    assert_eq!(walk.reached.len(), 3);
    assert_eq!(walk.truncated.map(Truncation::reason), Some("scan_cap"));

    let exact = WalkLimits { depth: 1, node_cap: 10 };
    let walk = g.walk(&[hub], Direction::Importers, exact, Tiers::RUNTIME);
    assert_eq!(
        (walk.reached.len(), walk.truncated),
        (10, None),
        "a cap met exactly cuts nothing"
    );
}

#[test]
fn starts_and_self_loops_are_not_reported_by_a_walk() {
    let g = graph(&[("a", "a"), ("a", "b"), ("b", "a")]);
    let (a, b) = (id(&g, "a"), id(&g, "b"));
    let walk = g.walk(
        &[a, b, a],
        Direction::Dependencies,
        WalkLimits::new(Some(5)),
        Tiers::RUNTIME,
    );
    assert_eq!(walk, Walk::default(), "every edge leads back to a start");
    let unknown = g.walk(&[99], Direction::Dependencies, WalkLimits::new(None), Tiers::RUNTIME);
    assert_eq!(unknown, Walk::default(), "a node outside the graph reaches nothing");
}

#[test]
fn cycles_are_components_and_self_loops_in_path_order() {
    let g = graph(&[
        ("c", "a"),
        ("a", "b"),
        ("b", "c"),
        ("d", "d"),
        ("e", "f"),
        ("h", "g"),
        ("g", "h"),
        ("b", "e"),
    ]);
    assert_eq!(
        cycles(&g, Tiers::RUNTIME),
        [vec!["a", "b", "c"], vec!["d"], vec!["g", "h"]],
        "e and f sit on no cycle"
    );
    assert!(cycles(&diamond(), Tiers::RUNTIME).is_empty(), "a diamond is acyclic");
}

#[test]
fn the_cycle_search_is_iterative_on_a_long_chain() {
    let nodes: Vec<String> = (0..200_000).map(|n| format!("f{n:06}")).collect();
    let mut edges: Vec<(&str, &str)> = nodes
        .windows(2)
        .map(|pair| (pair[0].as_str(), pair[1].as_str()))
        .collect();
    let g = graph(&edges);
    assert!(g.cycles(Tiers::RUNTIME).is_empty());
    edges.push((nodes[nodes.len() - 1].as_str(), nodes[0].as_str()));
    let g = graph(&edges);
    let found = g.cycles(Tiers::RUNTIME);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].len(), nodes.len(), "the closed chain is one component");
}

#[test]
fn type_only_edges_count_only_when_asked() {
    let g = ImportGraph::build(
        rows(&[("a", &[("b", I)]), ("b", &[("a", T), ("c", T)])]),
        std::iter::empty(),
        |_| true,
        true,
    );
    let (a, b, c) = (id(&g, "a"), id(&g, "b"), id(&g, "c"));
    assert!(
        cycles(&g, Tiers::RUNTIME).is_empty(),
        "the type-only back edge closes no cycle"
    );
    assert_eq!(cycles(&g, TYPES), [vec!["a", "b"]]);
    assert_eq!(g.in_degree(c, Tiers::RUNTIME), 0);
    assert_eq!(g.in_degree(c, TYPES), 1);
    assert_eq!(g.dependencies(b, Tiers::RUNTIME).count(), 0);
    let walk = g.walk(&[b], Direction::Dependencies, WalkLimits::new(Some(2)), TYPES);
    assert_eq!(reached(&g, &walk), [("a".to_string(), 1), ("c".to_string(), 1)]);
    assert!(
        g.would_close_cycle(b, a, Tiers::RUNTIME),
        "a already imports b at runtime"
    );
    assert!(!g.would_close_cycle(a, c, Tiers::RUNTIME), "c imports nothing");
    assert!(
        g.would_close_cycle(c, b, TYPES) && !g.would_close_cycle(c, b, Tiers::RUNTIME),
        "b reaches c only through a type-only edge"
    );
    assert!(g.would_close_cycle(c, c, Tiers::RUNTIME), "a file importing itself");
}

/// A Rust parent module declares its children (`mod a; mod b;`) and each child uses an item of the
/// crate root (`use crate::X`): with the declarations as edges every child is in one cycle with the
/// root, without them there is none. A real `use` cycle between two children stays a cycle.
#[test]
fn module_declarations_are_left_out_of_cycles_and_walks_unless_asked() {
    let g = ImportGraph::build(
        rows(&[
            ("lib.rs", &[("a.rs", M), ("b.rs", M), ("c.rs", M.union(I))]),
            ("a.rs", &[("lib.rs", I), ("b.rs", I)]),
            ("b.rs", &[("lib.rs", I), ("a.rs", I)]),
            ("c.rs", &[("lib.rs", I)]),
        ]),
        std::iter::empty(),
        |_| true,
        true,
    );
    let (lib, a, b, c) = (id(&g, "lib.rs"), id(&g, "a.rs"), id(&g, "b.rs"), id(&g, "c.rs"));
    assert_eq!(
        cycles(&g, Tiers::RUNTIME),
        [vec!["a.rs", "b.rs"], vec!["c.rs", "lib.rs"]],
        "a and b use each other; lib.rs also uses c.rs (`pub use c::X`) beside declaring it"
    );
    let with_decls = Tiers {
        module_decls: true,
        ..Tiers::RUNTIME
    };
    assert_eq!(cycles(&g, with_decls), [vec!["a.rs", "b.rs", "c.rs", "lib.rs"]]);
    assert_eq!(cycles(&g, Tiers::ALL), cycles(&g, with_decls));

    assert_eq!(g.in_degree(a, Tiers::RUNTIME), 1, "only b uses a");
    assert_eq!(g.in_degree(a, with_decls), 2, "lib.rs declares it");
    assert_eq!(
        names(&g, g.dependencies(lib, Tiers::RUNTIME).map(|edge| edge.node)),
        ["c.rs"]
    );
    let walk = g.walk(&[c], Direction::Importers, WalkLimits::new(Some(5)), Tiers::RUNTIME);
    assert_eq!(
        reached(&g, &walk),
        [
            ("lib.rs".to_string(), 1),
            ("a.rs".to_string(), 2),
            ("b.rs".to_string(), 2)
        ],
        "lib.rs uses c; a and b use lib.rs"
    );
    let walk = g.walk(&[a], Direction::Importers, WalkLimits::new(Some(5)), Tiers::RUNTIME);
    assert_eq!(
        reached(&g, &walk),
        [("b.rs".to_string(), 1)],
        "lib.rs only declares a, so it is no importer of a"
    );
    let walk = g.walk(&[lib], Direction::Importers, WalkLimits::new(Some(1)), Tiers::RUNTIME);
    assert_eq!(
        names(&g, walk.reached.iter().map(|hit| hit.node)),
        ["a.rs", "b.rs", "c.rs"]
    );
    let walk = g.walk(
        &[lib],
        Direction::Dependencies,
        WalkLimits::new(Some(5)),
        Tiers::RUNTIME,
    );
    assert_eq!(
        names(&g, walk.reached.iter().map(|hit| hit.node)),
        ["c.rs"],
        "declared modules are not dependencies by default"
    );
    let walk = g.walk(&[lib], Direction::Dependencies, WalkLimits::new(Some(1)), with_decls);
    assert_eq!(
        names(&g, walk.reached.iter().map(|hit| hit.node)),
        ["a.rs", "b.rs", "c.rs"]
    );
    assert!(
        !g.would_close_cycle(a, c, Tiers::RUNTIME) && g.would_close_cycle(a, c, with_decls),
        "c reaches a only through lib.rs declaring it"
    );
    assert!(g.would_close_cycle(b, a, Tiers::RUNTIME), "a already uses b");
}

#[test]
fn reachability_follows_every_depth() {
    let g = graph(&[("main", "a"), ("a", "b"), ("b", "c"), ("dead", "c")]);
    let mask = g.reachable(&[id(&g, "main")], Direction::Dependencies, Tiers::RUNTIME);
    let live: Vec<String> = names(
        &g,
        (0..g.len()).filter(|&n| mask[n]).map(|n| NodeId::try_from(n).unwrap()),
    );
    assert_eq!(
        live,
        ["a", "b", "c", "main"],
        "`dead` is unreachable from the entry point"
    );
}

#[test]
fn coverage_counts_per_language_and_names_the_unresolved_ones() {
    let mut edges = rows(&[("src/app.ts", &[("src/lib.ts", I), ("src/types.ts", T)])]);
    edges[0].1.external = 3;
    edges[0].1.unresolved = 1;
    edges[0].1.unresolved_specifiers = vec!["./gone".to_string()];
    let files: Vec<(RelPath, &str)> = [
        ("src/app.ts", "typescript"),
        ("src/lib.ts", "typescript"),
        ("src/types.ts", "typescript"),
        ("tool.rb", "ruby"),
        ("other.rb", "ruby"),
    ]
    .into_iter()
    .map(|(path, language)| (RelPath::from(path), language))
    .collect();
    let g = ImportGraph::build(
        edges,
        files.iter().map(|(path, language)| (path, *language)),
        |language| language == "typescript",
        false,
    );
    assert!(!g.probes_complete());
    let coverage = g.coverage();
    assert_eq!(
        coverage.languages,
        [
            LanguageCoverage {
                language: "ruby".to_string(),
                resolver: false,
                files: 2,
                ..LanguageCoverage::default()
            },
            LanguageCoverage {
                language: "typescript".to_string(),
                resolver: true,
                files: 3,
                importers: 1,
                edges: 2,
                type_only_edges: 1,
                module_decl_edges: 0,
                external: 3,
                unresolved: 1,
            },
        ]
    );
    assert_eq!(coverage.without_resolver().collect::<Vec<_>>(), ["ruby"]);
    assert!(coverage.any_resolvable());

    let unanalysed = ImportGraph::build(
        Vec::new(),
        files.iter().map(|(path, language)| (path, *language)),
        |_| false,
        true,
    );
    assert!(unanalysed.is_empty() && !unanalysed.coverage().any_resolvable());
}

#[test]
fn the_cache_keeps_one_generation_and_never_goes_back() {
    let cache = GraphCache::default();
    let builds = Cell::new(0);
    let build = |edges: &'static [(&'static str, &'static str)]| {
        builds.set(builds.get() + 1);
        Ok::<_, EngineError>(graph(edges))
    };
    let first = cache.get_or_build(1, || build(&[("a", "b")])).unwrap();
    let again = cache.get_or_build(1, || build(&[("x", "y")])).unwrap();
    assert!(Arc::ptr_eq(&first, &again), "one build per generation");
    assert_eq!(builds.get(), 1);

    let newer = cache.get_or_build(2, || build(&[("a", "c")])).unwrap();
    assert!(newer.node("c").is_some(), "a newer generation builds again");
    let older = cache.get_or_build(1, || build(&[("a", "d")])).unwrap();
    assert!(
        older.node("d").is_some(),
        "a late caller still gets its own generation's graph"
    );
    let kept = cache.get_or_build(2, || build(&[("q", "r")])).unwrap();
    assert!(
        Arc::ptr_eq(&newer, &kept),
        "the older build did not replace the newer graph"
    );
    assert_eq!(builds.get(), 3);

    let failed = cache.get_or_build(3, || Err(EngineError::internal("boom")));
    assert!(failed.is_err(), "a failed build is the caller's error");
    assert!(Arc::ptr_eq(&cache.get_or_build(2, || build(&[])).unwrap(), &newer));
    assert!(newer.approx_bytes() > 0);
}

/// A workspace over `root`, scanned in place, as the in-process serve opens one.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
fn scanned_workspace(root: &std::path::Path) -> Workspace {
    let store = crate::store::Store::open(root, crate::store::VIEW_WORKING).expect("store");
    let ws = Workspace::new(
        store,
        root.to_path_buf(),
        Arc::new(crate::config::Config::with_defaults()),
        None,
        Arc::new(crate::git_cache::GitCache::open(root, 1, false).expect("git cache")),
        None,
        crate::engine::workspace::OpenOptions {
            background: false,
            read_only: false,
            forwards_writes: false,
            lazy_cache: false,
            history_sync: None,
            #[cfg(feature = "memory")]
            memory_owner: |_| "owner".to_string(),
        },
    );
    ws.rescan(None, crate::scanner::EmbedMode::Deferred, false)
        .expect("scan");
    ws
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
fn edges_of(graph: &ImportGraph) -> Vec<(String, String)> {
    (0..graph.len())
        .map(|n| NodeId::try_from(n).unwrap())
        .flat_map(|from| {
            graph
                .dependencies(from, Tiers::ALL)
                .map(move |edge| (graph.path(from).to_string(), graph.path(edge.node).to_string()))
        })
        .collect()
}

/// The graph of a scanned Rust crate (and a JS pair under `code-intel-js`), kept while the index
/// stands and built again after a rescan changes it.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
#[test]
fn a_scanned_fixture_builds_the_graph_and_a_rescan_rebuilds_it() {
    crate::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("basemind.toml"), "").unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "mod a;\nmod b;\n").unwrap();
    std::fs::write(root.join("src/a.rs"), "use crate::b::thing;\npub fn f() { thing(); }\n").unwrap();
    std::fs::write(root.join("src/b.rs"), "pub fn thing() {}\n").unwrap();
    #[cfg(feature = "code-intel-js")]
    {
        std::fs::create_dir_all(root.join("web")).unwrap();
        std::fs::write(
            root.join("web/x.js"),
            "import { y } from './y.js';\nexport const x = y + 1;\n",
        )
        .unwrap();
        std::fs::write(
            root.join("web/y.js"),
            "import { x } from './x.js';\nexport const y = 2;\n",
        )
        .unwrap();
    }
    let ws = scanned_workspace(root);

    let first = import_graph(&ws).expect("graph");
    assert!(first.probes_complete(), "the scan ran the resolve pass");
    let edges = edges_of(&first);
    for (from, to) in [
        ("src/a.rs", "src/b.rs"),
        ("src/lib.rs", "src/a.rs"),
        ("src/lib.rs", "src/b.rs"),
    ] {
        assert!(
            edges.contains(&(from.to_string(), to.to_string())),
            "{from} -> {to} in {edges:?}"
        );
    }
    let rust = first
        .coverage()
        .languages
        .iter()
        .find(|language| language.language == "rust")
        .expect("rust coverage");
    assert!(rust.resolver && rust.files == 3 && rust.edges >= 3, "{rust:?}");
    #[cfg(feature = "code-intel-js")]
    assert_eq!(cycles(&first, Tiers::RUNTIME), [vec!["web/x.js", "web/y.js"]]);
    assert!(
        Arc::ptr_eq(&first, &import_graph(&ws).expect("graph")),
        "an unchanged index serves the kept graph"
    );

    std::fs::write(root.join("src/c.rs"), "pub fn more() {}\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), "mod a;\nmod b;\nmod c;\n").unwrap();
    ws.rescan(None, crate::scanner::EmbedMode::Deferred, false)
        .expect("rescan");
    let second = import_graph(&ws).expect("graph");
    assert!(!Arc::ptr_eq(&first, &second), "the rescan invalidated the kept graph");
    assert!(
        edges_of(&second).contains(&("src/lib.rs".to_string(), "src/c.rs".to_string())),
        "the new module is an edge after the rescan"
    );
    assert!(first.node("src/c.rs").is_none());
}

/// Through a real scan: a parent declaring its children, which `use crate::…` back, forms no cycle;
/// two modules using each other do. With module declarations asked for, the parent and the child
/// using it are one.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
#[test]
fn a_scanned_module_tree_has_cycles_only_where_modules_use_each_other() {
    crate::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("basemind.toml"), "").unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "mod a;\nmod b;\nmod c;\npub struct Root;\n").unwrap();
    std::fs::write(root.join("src/a.rs"), "use crate::Root;\npub fn a() {}\n").unwrap();
    std::fs::write(root.join("src/b.rs"), "use crate::c::c;\npub fn b() {}\n").unwrap();
    std::fs::write(root.join("src/c.rs"), "use crate::b::b;\npub fn c() {}\n").unwrap();
    let ws = scanned_workspace(root);
    let graph = import_graph(&ws).expect("graph");
    assert_eq!(
        cycles(&graph, Tiers::RUNTIME),
        [vec!["src/b.rs", "src/c.rs"]],
        "lib.rs declaring a.rs, which uses lib.rs, is no cycle"
    );
    let with_decls = Tiers {
        module_decls: true,
        ..Tiers::RUNTIME
    };
    assert_eq!(
        cycles(&graph, with_decls),
        [vec!["src/a.rs", "src/lib.rs"], vec!["src/b.rs", "src/c.rs"]]
    );
    let rust = graph
        .coverage()
        .languages
        .iter()
        .find(|language| language.language == "rust")
        .expect("rust coverage");
    assert_eq!(rust.module_decl_edges, 3, "{rust:?}");
}

/// Build time and size of the graph of the repository at `BASEMIND_GRAPH_MEASURE_ROOT`, scanned
/// into a throwaway data home. Run in release with `--ignored --nocapture`.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
#[test]
#[ignore = "measurement: needs BASEMIND_GRAPH_MEASURE_ROOT and a release build"]
fn measure_the_graph_of_a_repository() {
    let Some(root) = std::env::var_os("BASEMIND_GRAPH_MEASURE_ROOT") else {
        panic!("set BASEMIND_GRAPH_MEASURE_ROOT to the repository to measure");
    };
    crate::store::init_isolated_cache();
    let root = std::path::PathBuf::from(root);
    let scan_started = std::time::Instant::now();
    let ws = scanned_workspace(&root);
    let scan = scan_started.elapsed();
    let mut builds = Vec::new();
    let mut graph = None;
    for _ in 0..5 {
        let started = std::time::Instant::now();
        let store = ws.store_read();
        let view = index_view(&store).expect("view");
        let files = store.index.files.iter().map(|(p, e)| (p, e.language.as_str()));
        let rows = view.file_edges();
        let rows_at = started.elapsed();
        graph = Some(ImportGraph::build(
            rows,
            files,
            has_resolver,
            view.import_probes_complete(),
        ));
        builds.push((rows_at, started.elapsed()));
    }
    let graph = graph.expect("built");
    let started = std::time::Instant::now();
    let cycles = graph.cycles(Tiers::RUNTIME);
    let tarjan = started.elapsed();
    println!(
        "root={} scan={scan:?} files={} nodes={} edges={} bytes={} cycles={} largest={} tarjan={tarjan:?}",
        root.display(),
        ws.store_read().index.files.len(),
        graph.len(),
        graph.edge_count(),
        graph.approx_bytes(),
        cycles.len(),
        cycles.iter().map(Vec::len).max().unwrap_or(0),
    );
    println!("builds (rows read, total): {builds:?}");
    println!("coverage: {:?}", graph.coverage());
    for (label, tiers) in [("runtime", Tiers::RUNTIME), ("with module_decl", Tiers::ALL)] {
        let mut found = graph.cycles(tiers);
        found.sort_by_key(|cycle| std::cmp::Reverse(cycle.len()));
        let sizes: Vec<usize> = found.iter().map(Vec::len).collect();
        println!("cycles {label}: {} components, sizes {sizes:?}", found.len());
        if tiers == Tiers::RUNTIME {
            for cycle in found.iter().take(15) {
                let shown: Vec<String> = cycle
                    .iter()
                    .take(40)
                    .map(|&node| graph.path(node).to_string())
                    .collect();
                println!("  [{}] {}", cycle.len(), shown.join(" "));
            }
        }
    }
    let store = ws.store_read();
    let view = index_view(&store).expect("view");
    let mut heads: AHashMap<String, u32> = AHashMap::new();
    for (_, edges) in view.file_edges() {
        for specifier in &edges.unresolved_specifiers {
            let head = specifier.split("::").next().unwrap_or(specifier).to_string();
            *heads.entry(head).or_default() += 1;
        }
    }
    let mut heads: Vec<(String, u32)> = heads.into_iter().collect();
    heads.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    println!(
        "unresolved specifier heads (files): {:?}",
        &heads[..heads.len().min(40)]
    );
}
