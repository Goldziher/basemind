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
