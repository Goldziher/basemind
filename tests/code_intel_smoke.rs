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
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let src = "const count = 1;\nfunction f() {\n  return count + count;\n}\n";
    fs::write(root.join("app.js"), src).unwrap();

    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    let cfg = ConfigV1::with_defaults();
    scan(
        root,
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .unwrap();

    if store.lookup("app.js").is_none() {
        eprintln!("javascript grammar unavailable in this environment — skipping resolution assertions");
        return;
    }

    let app = RelPath::from("app.js");
    let def_start = (src.find("const count").unwrap() + "const ".len()) as u32;

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
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let a_src = "export function helper() {\n  return 1;\n}\n";
    let b_src = "import { helper } from './a';\nexport function run() {\n  return helper();\n}\n";
    fs::write(root.join("a.ts"), a_src).unwrap();
    fs::write(root.join("b.ts"), b_src).unwrap();

    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    let cfg = ConfigV1::with_defaults();
    scan(
        root,
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .unwrap();

    if store.lookup("a.ts").is_none() || store.lookup("b.ts").is_none() {
        eprintln!("typescript grammar unavailable in this environment — skipping cross-file assertions");
        return;
    }

    let a = RelPath::from("a.ts");
    let export_name_start = (a_src.find("function helper").unwrap() + "function ".len()) as u32;
    let import_local_start = b_src.find("helper").unwrap() as u32;

    let uses = basemind::query::resolved_references(&store, &a, export_name_start);
    assert!(
        uses.iter()
            .any(|(p, s)| p.as_str() == Some("b.ts") && *s == import_local_start),
        "the `helper` import in b.ts must resolve to the a.ts export at {export_name_start}; got {uses:?}"
    );

    let b = RelPath::from("b.ts");
    let def = basemind::query::definition_of(&store, &b, import_local_start);
    assert_eq!(
        def,
        Some((a.clone(), export_name_start)),
        "cross-file goto-definition: the b.ts import binding must resolve to the a.ts export"
    );
}

#[test]
fn resolved_references_do_not_conflate_same_named_symbols_across_files() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let a_src = "const count = 1;\nfunction fa() {\n  return count;\n}\n";
    let b_src = "const count = 2;\nfunction fb() {\n  return count;\n}\n";
    fs::write(root.join("a.js"), a_src).unwrap();
    fs::write(root.join("b.js"), b_src).unwrap();

    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    let cfg = ConfigV1::with_defaults();
    scan(
        root,
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .unwrap();

    if store.lookup("a.js").is_none() || store.lookup("b.js").is_none() {
        eprintln!("javascript grammar unavailable in this environment — skipping no-conflation assertions");
        return;
    }

    let a = RelPath::from("a.js");
    let a_count_def = (a_src.find("const count").unwrap() + "const ".len()) as u32;
    let uses = basemind::query::resolved_references(&store, &a, a_count_def);
    assert!(
        !uses.is_empty() && uses.iter().all(|(p, _)| p.as_str() == Some("a.js")),
        "a.js `count` must resolve only within a.js, never to b.js's unrelated `count`; got {uses:?}"
    );
}
