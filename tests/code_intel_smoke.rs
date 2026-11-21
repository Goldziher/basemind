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
        basemind::retrieval_wiring::backend(),
    )
    .unwrap();

    if store.lookup("app.js").is_none() {
        basemind::testing::unavailable(
            "BASEMIND_TEST_ALLOW_NO_GRAMMAR",
            "the javascript grammar is unavailable",
        );
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
        basemind::retrieval_wiring::backend(),
    )
    .unwrap();

    if store.lookup("a.ts").is_none() || store.lookup("b.ts").is_none() {
        basemind::testing::unavailable(
            "BASEMIND_TEST_ALLOW_NO_GRAMMAR",
            "the typescript grammar is unavailable",
        );
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
        basemind::retrieval_wiring::backend(),
    )
    .unwrap();

    if store.lookup("a.js").is_none() || store.lookup("b.js").is_none() {
        basemind::testing::unavailable(
            "BASEMIND_TEST_ALLOW_NO_GRAMMAR",
            "the javascript grammar is unavailable",
        );
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

/// Scan a repo of `files`, or `None` (after reporting the missing grammar) when any is unindexed.
fn scanned(files: &[(&str, &str)]) -> Option<(tempfile::TempDir, Store)> {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().unwrap();
    for (rel, body) in files {
        fs::write(dir.path().join(rel), body).unwrap();
    }
    let mut store = Store::open(dir.path(), VIEW_WORKING).unwrap();
    scan(
        dir.path(),
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .unwrap();
    if files.iter().any(|(rel, _)| store.lookup(rel).is_none()) {
        basemind::testing::unavailable(
            "BASEMIND_TEST_ALLOW_NO_GRAMMAR",
            "the typescript grammar is unavailable",
        );
        return None;
    }
    Some((dir, store))
}

/// An importer that imports `name` from `./index` and calls it once.
fn app_importing(name: &str) -> String {
    format!("import {{ {name} }} from './index';\nexport const run = () => {name}();\n")
}

/// Every use of the definition at `needle`'s `name` in `def_file`, as sorted `(file, byte)` pairs.
fn uses_of(store: &Store, files: &[(&str, &str)], def_file: &str, needle: &str, name: &str) -> Vec<(String, u32)> {
    let src = files.iter().find(|(rel, _)| *rel == def_file).unwrap().1;
    let def_start = (src.find(needle).unwrap() + needle.find(name).unwrap()) as u32;
    let mut uses: Vec<(String, u32)> = basemind::query::resolved_references(store, &RelPath::from(def_file), def_start)
        .into_iter()
        .map(|(path, start)| (path.as_str().unwrap().to_string(), start))
        .collect();
    uses.sort_unstable();
    uses
}

/// `app.ts` imports `name` through barrels and must resolve, at its import binding and its call, to
/// `function helper` in `impl.ts`.
fn assert_app_resolves_to_impl(files: &[(&str, &str)], name: &str) {
    let Some((_dir, store)) = scanned(files) else {
        return;
    };
    let app = files.iter().find(|(rel, _)| *rel == "app.ts").unwrap().1;
    let import_start = (app.find(&format!("{{ {name} }}")).unwrap() + 2) as u32;
    let call_start = app.find(&format!("{name}()")).unwrap() as u32;
    assert_eq!(
        uses_of(&store, files, "impl.ts", "function helper", "helper"),
        [("app.ts".to_string(), import_start), ("app.ts".to_string(), call_start)],
        "the import of `{name}` and its call must resolve through the barrels to impl.ts"
    );
    let impl_src = files.iter().find(|(rel, _)| *rel == "impl.ts").unwrap().1;
    let def_start = (impl_src.find("function helper").unwrap() + "function ".len()) as u32;
    assert_eq!(
        basemind::query::definition_of(&store, &RelPath::from("app.ts"), import_start),
        Some((RelPath::from("impl.ts"), def_start)),
        "goto-definition of the `{name}` import binding must land in impl.ts"
    );
}

const IMPL: &str = "export function helper() {\n  return 1;\n}\n";

#[test]
fn imports_resolve_through_one_reexport_barrel() {
    for (barrel, name) in [
        ("export { helper } from './impl';\n", "helper"),
        ("export * from './impl';\n", "helper"),
        ("export { helper as tool } from './impl';\n", "tool"),
    ] {
        let app = app_importing(name);
        assert_app_resolves_to_impl(
            &[("impl.ts", IMPL), ("index.ts", barrel), ("app.ts", app.as_str())],
            name,
        );
    }
}

#[test]
fn imports_resolve_through_two_reexport_barrels() {
    let app = app_importing("tool");
    assert_app_resolves_to_impl(
        &[
            ("impl.ts", IMPL),
            ("mid.ts", "export { helper as tool } from './impl';\n"),
            ("index.ts", "export * from './mid';\n"),
            ("app.ts", app.as_str()),
        ],
        "tool",
    );
}

#[test]
fn star_reexport_does_not_forward_the_default_export() {
    let files = [
        (
            "impl.ts",
            "export function helper() {\n  return 1;\n}\nexport { helper as default };\n",
        ),
        ("index.ts", "export * from './impl';\n"),
        (
            "app.ts",
            "import helper from './index';\nexport const run = () => helper();\n",
        ),
    ];
    let Some((_dir, store)) = scanned(&files) else {
        return;
    };
    let import_start = files[2].1.find("helper").unwrap() as u32;
    assert_eq!(
        basemind::query::definition_of(&store, &RelPath::from("app.ts"), import_start),
        None,
        "`export * from` forwards every name but `default`"
    );
}

#[test]
fn reexport_cycle_terminates_and_the_search_continues_past_it() {
    let files = [
        ("index.ts", "export * from './loop';\nexport * from './impl';\n"),
        ("loop.ts", "export * from './index';\n"),
        ("impl.ts", IMPL),
        (
            "app.ts",
            "import { helper, missing } from './index';\nexport const run = () => helper() + missing;\n",
        ),
    ];
    let Some((_dir, store)) = scanned(&files) else {
        return;
    };
    let app = files[3].1;
    let import_start = app.find("helper").unwrap() as u32;
    let call_start = app.find("helper()").unwrap() as u32;
    assert_eq!(
        uses_of(&store, &files, "impl.ts", "function helper", "helper"),
        [("app.ts".to_string(), import_start), ("app.ts".to_string(), call_start)],
        "the cycle through loop.ts must not stop the search reaching impl.ts"
    );
    let missing_start = app.find("missing").unwrap() as u32;
    assert_eq!(
        basemind::query::definition_of(&store, &RelPath::from("app.ts"), missing_start),
        None,
        "a name no file in the cycle defines resolves to nothing"
    );
}

#[test]
fn a_named_reexport_beats_a_local_import_of_the_same_name() {
    let app = app_importing("helper");
    assert_app_resolves_to_impl(
        &[
            ("other.ts", "export function helper() {\n  return 2;\n}\n"),
            ("impl.ts", IMPL),
            (
                "index.ts",
                "import { helper } from './other';\nexport { helper } from './impl';\n",
            ),
            ("app.ts", app.as_str()),
        ],
        "helper",
    );
}

#[test]
fn a_named_reexport_of_an_unindexed_module_shadows_a_star_reexport() {
    let files = [
        ("impl.ts", IMPL),
        (
            "index.ts",
            "export { helper } from '../external';\nexport * from './impl';\n",
        ),
        (
            "app.ts",
            "import { helper } from './index';\nexport const run = () => helper();\n",
        ),
    ];
    let Some((_dir, store)) = scanned(&files) else {
        return;
    };
    let import_start = files[2].1.find("helper").unwrap() as u32;
    assert_eq!(
        basemind::query::definition_of(&store, &RelPath::from("app.ts"), import_start),
        None,
        "the named re-export binds `helper`; the star re-export is not consulted"
    );
}

#[test]
fn a_shorter_path_searches_again_a_barrel_the_hop_limit_cut_on_a_longer_one() {
    let mut barrels: Vec<(String, String)> = (1..6)
        .map(|n| (format!("l{n}.ts"), format!("export * from './l{}';\n", n + 1)))
        .collect();
    barrels.push(("l6.ts".to_string(), "export * from './shared';\n".to_string()));
    let app = app_importing("helper");
    let mut files: Vec<(&str, &str)> = barrels
        .iter()
        .map(|(rel, body)| (rel.as_str(), body.as_str()))
        .collect();
    files.extend([
        ("index.ts", "export * from './l1';\nexport * from './shared';\n"),
        ("shared.ts", "export * from './impl';\n"),
        ("impl.ts", IMPL),
        ("app.ts", app.as_str()),
    ]);
    assert_app_resolves_to_impl(&files, "helper");
}
