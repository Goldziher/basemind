//! End-to-end code-intelligence smoke: a full `scan` must persist scope-resolved reference
//! edges that `query::resolved_references` / `query::definition_of` read back.
//!
//! Gated on `code-intel-js` (the oxc engine). The scan's L1 pass still needs the JavaScript
//! tree-sitter grammar; if that grammar can't be fetched in this environment (cold TSLP cache),
//! the file isn't indexed and the test skips its assertions rather than failing spuriously —
//! resolution itself is grammar-free (oxc), but a file must be indexed for the resolve pass to
//! see it.
#![cfg(feature = "code-intel-js")]

use std::fs;

use basemind::config::ConfigV1;
use basemind::path::RelPath;
use basemind::scanner::{ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};

#[test]
fn scan_resolves_intra_file_references_for_javascript() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // `count` is a module-level const used twice inside `f`. Name-based matching would conflate
    // it with any other `count`; the resolved edge must point at this exact definition.
    let src = "const count = 1;\nfunction f() {\n  return count + count;\n}\n";
    fs::write(root.join("app.js"), src).unwrap();

    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    let cfg = ConfigV1::with_defaults();
    scan(root, &mut store, &cfg, ScanSource::WorkingTree).unwrap();

    if store.lookup("app.js").is_none() {
        eprintln!("javascript grammar unavailable in this environment — skipping resolution assertions");
        return;
    }

    let app = RelPath::from("app.js");
    let def_start = (src.find("const count").unwrap() + "const ".len()) as u32;

    // find_references: both uses of `count` resolve to the const definition, in this file.
    let mut uses = basemind::query::resolved_references(&store, &app, def_start);
    uses.sort_by_key(|(_, s)| *s);
    assert_eq!(
        uses.len(),
        2,
        "both uses of `count` must resolve to the const; got {uses:?}"
    );
    assert!(
        uses.iter().all(|(p, _)| p.as_str() == Some("app.js")),
        "resolved uses must be in app.js"
    );

    // goto_definition: the first `count` use resolves back to the const definition.
    let first_use = (src.find("return count").unwrap() + "return ".len()) as u32;
    let def = basemind::query::definition_of(&store, &app, first_use);
    assert_eq!(
        def,
        Some((app.clone(), def_start)),
        "goto-definition of the use must point at the const definition"
    );
}

#[test]
fn scan_resolves_cross_file_references_for_typescript() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // `a.ts` exports `helper`; `b.ts` imports and calls it. The cross-file stitch must link the
    // import binding in `b.ts` back to the `helper` export site in `a.ts`.
    let a_src = "export function helper() {\n  return 1;\n}\n";
    let b_src = "import { helper } from './a';\nexport function run() {\n  return helper();\n}\n";
    fs::write(root.join("a.ts"), a_src).unwrap();
    fs::write(root.join("b.ts"), b_src).unwrap();

    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    let cfg = ConfigV1::with_defaults();
    scan(root, &mut store, &cfg, ScanSource::WorkingTree).unwrap();

    // Both files must be indexed: the resolve pass only sees indexed files, and the join's
    // `store.lookup` gate drops unindexed targets. The TS grammar may be cold in a sandbox, so
    // skip (rather than fail) when either file didn't index — resolution itself is grammar-free.
    if store.lookup("a.ts").is_none() || store.lookup("b.ts").is_none() {
        eprintln!("typescript grammar unavailable in this environment — skipping cross-file assertions");
        return;
    }

    let a = RelPath::from("a.ts");
    // The `helper` export name-site in a.ts (the identifier in `export function helper`).
    let export_name_start = (a_src.find("function helper").unwrap() + "function ".len()) as u32;
    // The `helper` import binding site in b.ts (the local name in the import clause).
    let import_local_start = b_src.find("helper").unwrap() as u32;

    let uses = basemind::query::resolved_references(&store, &a, export_name_start);
    assert!(
        uses.iter()
            .any(|(p, s)| p.as_str() == Some("b.ts") && *s == import_local_start),
        "the `helper` import in b.ts must resolve to the a.ts export at {export_name_start}; got {uses:?}"
    );
}
