//! The file-level import graph at the data level: after a full scan, `IndexView::file_edges()`
//! holds each importer's first-hop target files and its external and unresolved import counts.
//!
//! Each case scans a small repository written to a temporary directory and asserts the exact row
//! of each importer, so an empty graph never passes. Cases skip (rather than fail) when a grammar
//! cannot be fetched.
#![cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]

use std::fs;

use basemind::config::ConfigV1;
use basemind::index::keys::FileEdges;
use basemind::index::view::IndexView;
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};

fn scanned(files: &[(&str, &str)]) -> (tempfile::TempDir, Store) {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().unwrap();
    for (rel, body) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
    }
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.embed = false;
    cfg.code_search.embed = false;
    let mut store = Store::open(dir.path(), VIEW_WORKING).unwrap();
    scan(
        dir.path(),
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .unwrap();
    (dir, store)
}

/// Whether `rel` was indexed; reports the skip when its grammar is unavailable.
fn indexed(store: &Store, rel: &str) -> bool {
    if store.lookup(rel).is_some() {
        return true;
    }
    basemind::testing::unavailable(
        "BASEMIND_TEST_ALLOW_NO_GRAMMAR",
        &format!("the grammar for {rel} is unavailable"),
    );
    false
}

/// Every importer's row, by importer path.
fn rows(store: &Store) -> Vec<(String, FileEdges)> {
    let mut rows: Vec<(String, FileEdges)> = store
        .index_db
        .as_ref()
        .unwrap()
        .view()
        .file_edges()
        .into_iter()
        .map(|(importer, edges)| (importer.as_str().unwrap().to_string(), edges))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// The row of `importer`, failing with every row when it has none.
fn row<'a>(rows: &'a [(String, FileEdges)], importer: &str) -> &'a FileEdges {
    rows.iter()
        .find(|(rel, _)| rel == importer)
        .map(|(_, edges)| edges)
        .unwrap_or_else(|| panic!("{importer} has no file-edge row; rows: {rows:#?}"))
}

fn targets(edges: &FileEdges) -> Vec<&str> {
    edges.edges.iter().map(|edge| edge.target.as_str().unwrap()).collect()
}

#[cfg(feature = "code-intel-js")]
mod js {
    use super::*;

    const APP: &str = "import './polyfill';\nimport * as ns from './ns';\nexport * from './all';\n\
                       export { n } from './named';\nexport type { T } from './types';\nimport 'left-pad';\n\
                       import './missing';\nexport const v = ns.z;\n";

    /// Side-effect imports, namespace imports and `export … from` re-exports bind no name, and
    /// each is still a file edge.
    #[test]
    fn module_only_imports_are_file_edges() {
        let (_dir, store) = scanned(&[
            ("polyfill.ts", "(globalThis as any).ready = true;\n"),
            ("ns.ts", "export const z = 1;\n"),
            ("all.ts", "export const a = 1;\n"),
            ("named.ts", "export const n = 1;\n"),
            ("types.ts", "export type T = number;\n"),
            ("app.ts", APP),
        ]);
        if !indexed(&store, "app.ts") {
            return;
        }
        let rows = rows(&store);
        let app = row(&rows, "app.ts");
        assert_eq!(targets(app), ["all.ts", "named.ts", "ns.ts", "polyfill.ts", "types.ts"]);
        let type_only: Vec<&str> = app
            .edges
            .iter()
            .filter(|edge| edge.type_only)
            .map(|edge| edge.target.as_str().unwrap())
            .collect();
        assert_eq!(type_only, ["types.ts"]);
        let polyfill = app
            .edges
            .iter()
            .find(|edge| edge.target.as_str() == Some("polyfill.ts"))
            .unwrap();
        assert_eq!(polyfill.local_start as usize, APP.find("'./polyfill'").unwrap());
        assert_eq!(app.external, 1, "left-pad is external");
        assert_eq!(app.unresolved, 1);
        assert_eq!(app.unresolved_specifiers, ["./missing"]);
    }
}

/// Targets of `importer`'s row with its external and unresolved counts.
fn summary(rows: &[(String, FileEdges)], importer: &str) -> (Vec<String>, u32, u32) {
    let edges = row(rows, importer);
    (
        targets(edges).into_iter().map(str::to_string).collect(),
        edges.external,
        edges.unresolved,
    )
}

fn owned(rels: &[&str]) -> Vec<String> {
    rels.iter().map(|rel| (*rel).to_string()).collect()
}

mod rust {
    use super::*;

    const B: &str = "use super::a::Thing;\nuse std::fmt;\nmod c;\npub fn helper() {}\n\n#[cfg(test)]\nmod tests {\n    \
                     use super::*;\n}\n";

    /// A `mod` tree with `crate::` and `super::` paths, a module cycle (`a` and `b` import each
    /// other), an inline test module and a declaration whose file is missing.
    #[test]
    fn mod_tree_and_use_paths_are_file_edges() {
        let (_dir, store) = scanned(&[
            ("Cargo.toml", "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n"),
            ("src/lib.rs", "pub mod a;\npub mod b;\nmod gone;\npub use a::Thing;\n"),
            (
                "src/a.rs",
                "use crate::b::helper;\npub struct Thing;\npub fn run() {\n    helper();\n}\n",
            ),
            ("src/b/mod.rs", B),
            ("src/b/c.rs", "use crate::Thing;\n"),
        ]);
        if !indexed(&store, "src/lib.rs") {
            return;
        }
        let rows = rows(&store);
        assert_eq!(
            summary(&rows, "src/lib.rs"),
            (owned(&["src/a.rs", "src/b/mod.rs"]), 0, 1)
        );
        assert_eq!(row(&rows, "src/lib.rs").unresolved_specifiers, ["mod gone"]);
        assert_eq!(summary(&rows, "src/a.rs"), (owned(&["src/b/mod.rs"]), 0, 0));
        assert_eq!(
            summary(&rows, "src/b/mod.rs"),
            (owned(&["src/a.rs", "src/b/c.rs"]), 1, 0),
            "`use super::*` in the inline test module is the file itself, not an edge"
        );
        assert_eq!(summary(&rows, "src/b/c.rs"), (owned(&["src/lib.rs"]), 0, 0));
        let to_c = row(&rows, "src/b/mod.rs")
            .edges
            .iter()
            .find(|edge| edge.target.as_str() == Some("src/b/c.rs"))
            .unwrap();
        assert_eq!(to_c.local_start as usize, B.find("mod c;").unwrap());
    }
}

mod go {
    use super::*;

    const MAIN: &str = "package main\n\nimport (\n\t\"fmt\"\n\t\"example.com/demo/store\"\n\t\"example.com/demo/missing\"\n)\n\n\
                        func main() { fmt.Println(store.Get()) }\n";

    /// An import of a package of the module is an edge to each of its non-test files.
    #[test]
    fn package_imports_are_edges_to_the_package_files() {
        let (_dir, store) = scanned(&[
            ("go.mod", "module example.com/demo\n\ngo 1.22\n"),
            ("main.go", MAIN),
            (
                "store/a.go",
                "package store\n\nimport \"example.com/demo/util\"\n\nfunc Get() int { return util.One() }\n",
            ),
            ("store/b.go", "package store\n\nfunc other() {}\n"),
            (
                "store/b_test.go",
                "package store\n\nimport \"testing\"\n\nfunc TestOther(t *testing.T) { other() }\n",
            ),
            ("util/u.go", "package util\n\nfunc One() int { return 1 }\n"),
        ]);
        if !indexed(&store, "main.go") {
            return;
        }
        let rows = rows(&store);
        assert_eq!(summary(&rows, "main.go"), (owned(&["store/a.go", "store/b.go"]), 1, 1));
        assert_eq!(
            row(&rows, "main.go").unresolved_specifiers,
            ["example.com/demo/missing"]
        );
        let anchors: Vec<usize> = row(&rows, "main.go")
            .edges
            .iter()
            .map(|edge| edge.local_start as usize)
            .collect();
        let quoted = MAIN.find("\"example.com/demo/store\"").unwrap();
        assert_eq!(anchors, [quoted, quoted]);
        assert_eq!(summary(&rows, "store/a.go"), (owned(&["util/u.go"]), 0, 0));
        assert_eq!(summary(&rows, "store/b_test.go"), (Vec::new(), 1, 0));
    }
}
