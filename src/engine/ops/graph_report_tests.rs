//! The shared parts of the graph commands over synthetic graphs, and the fixtures `impact` and
//! `cycles` tests build on.

use super::*;
use crate::index::keys::{EdgeKinds, FileEdge, FileEdges};

/// A graph of `(importer, target, kinds)` edges, every file in `language`, with a resolver for it
/// when `resolver`.
pub(in crate::engine::ops) fn graph_of(
    edges: &[(&str, &str, EdgeKinds)],
    language: &str,
    resolver: bool,
) -> ImportGraph {
    let mut rows: Vec<(RelPath, FileEdges)> = Vec::new();
    for &(importer, target, kinds) in edges {
        let importer = RelPath::from(importer);
        let at = match rows.iter().position(|(path, _)| *path == importer) {
            Some(at) => at,
            None => {
                rows.push((importer, FileEdges::default()));
                rows.len() - 1
            }
        };
        let edges = &mut rows[at].1.edges;
        let local_start = u32::try_from(edges.len()).unwrap() * 10;
        edges.push(FileEdge {
            target: RelPath::from(target),
            local_start,
            kinds,
        });
        edges.sort_unstable_by(|a, b| a.target.cmp(&b.target));
    }
    let mut files: Vec<RelPath> = edges
        .iter()
        .flat_map(|(a, b, _)| [RelPath::from(*a), RelPath::from(*b)])
        .collect();
    files.sort_unstable();
    files.dedup();
    let languages: Vec<(&RelPath, &str)> = files.iter().map(|path| (path, language)).collect();
    ImportGraph::build(rows, languages, |_| resolver, true)
}

pub(in crate::engine::ops) const I: EdgeKinds = EdgeKinds::IMPORT;
pub(in crate::engine::ops) const T: EdgeKinds = EdgeKinds::TYPE_ONLY;

pub(in crate::engine::ops) fn paths(graph: &ImportGraph, nodes: &[NodeId]) -> Vec<String> {
    nodes.iter().map(|&node| graph.path(node).to_string()).collect()
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
/// A workspace over `root`, a git repository when `git` holds, scanned in place.
pub(in crate::engine::ops) fn scanned_workspace(root: &std::path::Path, git: bool) -> Workspace {
    let store = crate::store::Store::open(root, crate::store::VIEW_WORKING).expect("store");
    let repo = git.then(|| std::sync::Arc::new(crate::git::Repo::discover(root).expect("repo")));
    let ws = Workspace::new(
        store,
        root.to_path_buf(),
        std::sync::Arc::new(crate::config::Config::with_defaults()),
        repo,
        std::sync::Arc::new(crate::git_cache::GitCache::open(root, 1, false).expect("git cache")),
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
pub(in crate::engine::ops) fn write(root: &std::path::Path, rel: &str, body: &str) {
    let file = root.join(rel);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(file, body).expect("write");
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
pub(in crate::engine::ops) fn git(root: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
/// A Rust crate whose `lib.rs` declares four modules: `a` and `b` use each other, `c` uses `a` and
/// `d` uses `c`. Line 1 of every module is its `use`.
pub(in crate::engine::ops) fn rust_crate(root: &std::path::Path) {
    write(root, "basemind.toml", "");
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    );
    write(root, "src/lib.rs", "mod a;\nmod b;\nmod c;\nmod d;\n");
    write(root, "src/a.rs", "use crate::b::bee;\npub fn ay() { bee(); }\n");
    write(
        root,
        "src/b.rs",
        "use crate::a::ay;\npub fn bee() {}\npub fn _x() { ay(); }\n",
    );
    write(root, "src/c.rs", "use crate::a::ay;\npub fn sea() { ay(); }\n");
    write(root, "src/d.rs", "use crate::c::sea;\npub fn dee() { sea(); }\n");
}

#[test]
fn a_shortest_chain_is_breadth_first_and_stays_within_the_component() {
    // a -> b -> c -> a and a -> d -> a: the shorter closes through d.
    let graph = graph_of(
        &[
            ("a", "b", I),
            ("b", "c", I),
            ("c", "a", I),
            ("a", "d", I),
            ("d", "a", I),
            ("d", "x", I),
        ],
        "rust",
        true,
    );
    let a = graph.node("a").unwrap();
    let chain = shortest_chain(&graph, a, Tiers::RUNTIME, |_| true, |node| node == a).unwrap();
    assert_eq!(paths(&graph, &chain), ["a", "d", "a"]);
    let b = graph.node("b").unwrap();
    let not_through_d = shortest_chain(
        &graph,
        a,
        Tiers::RUNTIME,
        |node| graph.path(node).as_bytes() != b"d",
        |node| node == a,
    )
    .unwrap();
    assert_eq!(paths(&graph, &not_through_d), ["a", "b", "c", "a"]);
    let x = graph.node("x").unwrap();
    assert_eq!(
        shortest_chain(&graph, x, Tiers::RUNTIME, |_| true, |node| node == b),
        None,
        "x imports nothing"
    );
}

#[test]
fn a_chain_follows_only_the_tiers_asked_for() {
    let graph = graph_of(&[("a", "b", I), ("b", "a", T)], "typescript", true);
    let a = graph.node("a").unwrap();
    assert_eq!(
        shortest_chain(&graph, a, Tiers::RUNTIME, |_| true, |node| node == a),
        None
    );
    let with_types = super::tiers(true, false);
    let chain = shortest_chain(&graph, a, with_types, |_| true, |node| node == a).unwrap();
    assert_eq!(paths(&graph, &chain), ["a", "b", "a"]);
}

#[test]
fn a_graph_with_no_resolvable_language_says_it_was_not_analysed() {
    let graph = graph_of(&[], "rust", false);
    assert_eq!(notes(&graph), ["no edges: the index holds no file"]);
    let unresolvable = ImportGraph::build(Vec::new(), [(&RelPath::from("x.zig"), "zig")], |_| false, true);
    assert_eq!(
        notes(&unresolvable),
        ["no edges: no resolver for zig in this build, so the graph is not analysed (not empty)"]
    );
    assert_eq!(coverage(&unresolvable).without_resolver, ["zig"]);
    assert!(coverage(&unresolvable).languages.is_empty());
}

#[test]
fn coverage_lists_resolved_languages_and_an_unfinished_pass_is_noted() {
    let graph = graph_of(&[("a.go", "b.go", I)], "go", true);
    let reported = coverage(&graph);
    assert_eq!((reported.files, reported.edges), (2, 1));
    assert_eq!(reported.languages.len(), 1);
    assert_eq!((reported.languages[0].files, reported.languages[0].edges), (2, 1));
    assert_eq!(notes(&graph), [GO_BUILD_TAGS]);

    let rows = vec![(
        RelPath::from("a.rs"),
        FileEdges {
            edges: vec![FileEdge {
                target: RelPath::from("b.rs"),
                local_start: 0,
                kinds: I,
            }],
            ..FileEdges::default()
        },
    )];
    let unfinished = ImportGraph::build(rows, [(&RelPath::from("a.rs"), "rust")], |_| true, false);
    assert_eq!(notes(&unfinished), [PROBES_INCOMPLETE]);
}
