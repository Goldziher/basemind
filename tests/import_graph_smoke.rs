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
