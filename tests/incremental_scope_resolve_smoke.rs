//! Incremental-scope cross-file resolution smoke: the watcher path (`scan_paths`) must re-stitch an
//! UNCHANGED importer when the dependency it imports changes — the reverse-import invariant.
//!
//! This is the risky half of the incremental resolve pass. `scan_paths` only re-resolves the files
//! it was told changed, so a naive implementation would leave `b.ts`'s cross-file edge dangling at
//! the OLD export site after `a.ts`'s export moved. The pass reconstructs the affected importer set
//! (every file importing a changed file) and re-stitches it, so editing ONLY `a.ts` must still move
//! `b.ts`'s edge to the new export site — and drop the stale edge at the old one.
//!
//! Gated on `code-intel-js` (the oxc engine). The L1 pass still needs the TypeScript tree-sitter
//! grammar; if it can't be fetched here (cold TSLP cache) the files aren't indexed and the test
//! skips its assertions rather than failing spuriously — mirrors the guard in `code_intel_smoke.rs`.
#![cfg(feature = "code-intel-js")]

use std::fs;

use basemind::config::ConfigV1;
use basemind::path::RelPath;
use basemind::scanner::{ScanSource, scan, scan_paths};
use basemind::store::{Store, VIEW_WORKING};

/// Byte offset of the `helper` identifier in `export function helper` for a given `a.ts` source.
fn helper_export_start(src: &str) -> u32 {
    (src.find("function helper").unwrap() + "function ".len()) as u32
}

#[test]
fn scan_paths_restitches_unchanged_importer_when_dependency_export_moves() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // a.ts exports `helper`; b.ts imports and calls it.
    let a_before = "export function helper() {\n  return 1;\n}\n";
    let b_src = "import { helper } from './a';\nexport function run() {\n  return helper();\n}\n";
    let a_abs = root.join("a.ts");
    fs::write(&a_abs, a_before).unwrap();
    fs::write(root.join("b.ts"), b_src).unwrap();

    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    let cfg = ConfigV1::with_defaults();
    scan(root, &mut store, &cfg, ScanSource::WorkingTree).unwrap();

    if store.lookup("a.ts").is_none() || store.lookup("b.ts").is_none() {
        eprintln!("typescript grammar unavailable in this environment — skipping cross-file assertions");
        return;
    }

    let a = RelPath::from("a.ts");
    let b = RelPath::from("b.ts");
    let import_local_start = b_src.find("helper").unwrap() as u32;
    let export_before = helper_export_start(a_before);

    // Baseline from the full scan: the cross-file edge links the b.ts import to the a.ts export.
    let baseline = basemind::query::resolved_references(&store, &a, export_before);
    assert!(
        baseline
            .iter()
            .any(|(p, s)| p.as_str() == Some("b.ts") && *s == import_local_start),
        "full scan must link b.ts's import to the a.ts export at {export_before}; got {baseline:?}"
    );

    // Move the export site: prepend a statement so `helper`'s `name_start` shifts. b.ts is NOT
    // touched. Re-scan ONLY a.ts via the incremental watcher entry point.
    let a_after = "const pad = 0;\nexport function helper() {\n  return pad + 2;\n}\n";
    fs::write(&a_abs, a_after).unwrap();
    scan_paths(root, &mut store, &cfg, &[a_abs]).unwrap();

    let export_after = helper_export_start(a_after);
    assert_ne!(
        export_before, export_after,
        "the edit must move the export name-site or the test proves nothing"
    );

    // The unchanged importer b.ts must have been re-stitched to the NEW export site, even though it
    // was not in the changed set handed to scan_paths.
    let after = basemind::query::resolved_references(&store, &a, export_after);
    assert!(
        after
            .iter()
            .any(|(p, s)| p.as_str() == Some("b.ts") && *s == import_local_start),
        "scan_paths(a.ts) must re-stitch b.ts's edge to the new a.ts export at {export_after}; got {after:?}"
    );

    // The stale edge at the OLD export site must be gone — the re-stitch replaces, never duplicates.
    let stale = basemind::query::resolved_references(&store, &a, export_before);
    assert!(
        !stale.iter().any(|(p, _)| p.as_str() == Some("b.ts")),
        "the stale edge at the old export site must be purged; got {stale:?}"
    );

    // goto-definition from the b.ts import binding now lands on the new a.ts export site.
    assert_eq!(
        basemind::query::definition_of(&store, &b, import_local_start),
        Some((a.clone(), export_after)),
        "cross-file goto-definition must follow the moved export after the incremental re-scan"
    );
}
