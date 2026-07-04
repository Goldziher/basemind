//! Incremental code-intelligence smoke: `scan_paths` (the watcher's per-file entry point) must
//! refresh scope-resolved reference edges the same way the full `scan` does. Two invariants:
//!
//! 1. Editing a file and re-scanning only that path re-runs the wholesale resolve pass, so
//!    `query::resolved_references` reflects the new use count.
//! 2. Deleting a file and re-scanning purges its resolved edges (`remove_resolved_file`), because
//!    a wholesale resolve pass iterates the CURRENT file set only and never revisits a removed
//!    file's stale `refs_by_def` / `refs_by_path` entries.
//!
//! Gated on `code-intel-js` (the oxc engine). The L1 pass still needs the JavaScript tree-sitter
//! grammar; if it can't be fetched here (cold TSLP cache) the file isn't indexed and the test skips
//! its assertions rather than failing spuriously — mirrors the guard in `code_intel_smoke.rs`.
#![cfg(feature = "code-intel-js")]

use std::fs;

use basemind::config::ConfigV1;
use basemind::path::RelPath;
use basemind::scanner::{ScanSource, scan, scan_paths};
use basemind::store::{Store, VIEW_WORKING};

/// `def_start` byte offset of the `count` binding in `const count = 1;` at file head — stable
/// across the edit because the edit only appends a use inside `f`.
fn count_def_start(src: &str) -> u32 {
    (src.find("const count").unwrap() + "const ".len()) as u32
}

#[test]
fn scan_paths_refreshes_resolved_edges_after_edit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Two uses of the module-level const `count` inside `f`.
    let before = "const count = 1;\nfunction f() {\n  return count + count;\n}\n";
    let app_abs = root.join("app.js");
    fs::write(&app_abs, before).unwrap();

    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    let cfg = ConfigV1::with_defaults();
    scan(root, &mut store, &cfg, ScanSource::WorkingTree).unwrap();

    if store.lookup("app.js").is_none() {
        eprintln!("javascript grammar unavailable in this environment — skipping resolution assertions");
        return;
    }

    let app = RelPath::from("app.js");
    let def_start = count_def_start(before);

    // Baseline from the full scan: two resolved uses.
    let baseline = basemind::query::resolved_references(&store, &app, def_start);
    assert_eq!(baseline.len(), 2, "full scan must resolve both uses; got {baseline:?}");

    // Edit: add a third use of `count`, then re-scan ONLY this path via the incremental entry point.
    let after = "const count = 1;\nfunction f() {\n  return count + count + count;\n}\n";
    fs::write(&app_abs, after).unwrap();
    scan_paths(root, &mut store, &cfg, &[app_abs]).unwrap();

    // The incremental resolve pass must have refreshed the edges — three uses now, all in app.js.
    let mut uses = basemind::query::resolved_references(&store, &app, def_start);
    uses.sort_by_key(|(_, s)| *s);
    assert_eq!(
        uses.len(),
        3,
        "scan_paths must re-run resolution and reflect the added use; got {uses:?}"
    );
    assert!(
        uses.iter().all(|(p, _)| p.as_str() == Some("app.js")),
        "resolved uses must stay in app.js"
    );

    // goto_definition stays consistent: the first use resolves back to the const.
    let first_use = (after.find("return count").unwrap() + "return ".len()) as u32;
    assert_eq!(
        basemind::query::definition_of(&store, &app, first_use),
        Some((app.clone(), def_start)),
        "goto-definition of a use must still point at the const after the incremental re-scan"
    );
}

#[test]
fn scan_paths_purges_resolved_edges_for_removed_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let src = "const count = 1;\nfunction f() {\n  return count + count;\n}\n";
    let keep_abs = root.join("keep.js");
    let gone_abs = root.join("gone.js");
    fs::write(&keep_abs, src).unwrap();
    fs::write(&gone_abs, src).unwrap();

    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    let cfg = ConfigV1::with_defaults();
    scan(root, &mut store, &cfg, ScanSource::WorkingTree).unwrap();

    if store.lookup("gone.js").is_none() || store.lookup("keep.js").is_none() {
        eprintln!("javascript grammar unavailable in this environment — skipping resolution assertions");
        return;
    }

    let keep = RelPath::from("keep.js");
    let gone = RelPath::from("gone.js");
    let def_start = count_def_start(src);

    // Both files carry resolved edges after the full scan.
    assert_eq!(basemind::query::resolved_references(&store, &gone, def_start).len(), 2);
    assert_eq!(basemind::query::resolved_references(&store, &keep, def_start).len(), 2);

    // Delete gone.js and re-scan only that path. The deletion-mirror loop must purge its resolved
    // edges; the wholesale resolve pass alone would never revisit them.
    fs::remove_file(&gone_abs).unwrap();
    scan_paths(root, &mut store, &cfg, &[gone_abs]).unwrap();

    assert!(
        basemind::query::resolved_references(&store, &gone, def_start).is_empty(),
        "removed file's resolved edges must be purged by scan_paths"
    );
    // The surviving file keeps its edges — the incremental scan is not destructive to others.
    assert_eq!(
        basemind::query::resolved_references(&store, &keep, def_start).len(),
        2,
        "surviving file's resolved edges must remain intact"
    );
}
