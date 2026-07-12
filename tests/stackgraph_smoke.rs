//! Cross-file resolution coverage for the stack-graph engine (Python / Java, `code-intel-stack`).
//!
//! `code_intel_smoke` covers the oxc JS/TS engine; this pins the stack-graph languages and, in
//! particular, the transitive re-export follow in the cross-file join (`intel::xfile`) that lets a
//! name imported through a package `__init__.py` resolve to the module that actually defines it —
//! the django `from django.db.models import QuerySet` pattern.
//!
//! Skips its assertions (rather than failing) when a grammar can't be fetched in this environment.
#![cfg(feature = "code-intel-stack")]

use std::fs;

use basemind::config::ConfigV1;
use basemind::path::RelPath;
use basemind::scanner::{ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};

/// Scan a set of `(relative_path, source)` files into an isolated store. Creates parent dirs so
/// package layouts (`pkg/query.py`) work.
fn scan_repo(files: &[(&str, &str)]) -> (tempfile::TempDir, Store) {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (name, src) in files {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, src).unwrap();
    }
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
    (dir, store)
}

/// A simple Python function and class, imported directly from the defining module, both resolve
/// cross-file to their uses in the importer.
#[test]
fn python_resolves_cross_file_function_and_class_on_direct_import() {
    let lib = "def foo():\n    return 1\n\n\nclass Bar:\n    pass\n";
    let main = "from lib import foo, Bar\nfoo()\nBar()\n";
    let (_d, store) = scan_repo(&[("lib.py", lib), ("main.py", main)]);
    if store.lookup("lib.py").is_none() {
        eprintln!("python grammar unavailable — skipping");
        return;
    }
    let lib_rel = RelPath::from("lib.py");
    let foo_def = (lib.find("def foo").unwrap() + "def ".len()) as u32;
    let bar_def = (lib.find("class Bar").unwrap() + "class ".len()) as u32;

    let foo_uses = basemind::query::resolved_references(&store, &lib_rel, foo_def);
    assert!(
        foo_uses.iter().any(|(p, _)| p.as_str() == Some("main.py")),
        "the Python function `foo` must resolve cross-file to main.py; got {foo_uses:?}"
    );
    let bar_uses = basemind::query::resolved_references(&store, &lib_rel, bar_def);
    assert!(
        bar_uses.iter().any(|(p, _)| p.as_str() == Some("main.py")),
        "the Python class `Bar` must resolve cross-file to main.py; got {bar_uses:?}"
    );
}

/// Regression for the django `QuerySet` gap: a class defined in `pkg/query.py`, re-exported by
/// `pkg/__init__.py`, and imported by a caller through the PACKAGE (`from pkg import QuerySet`) must
/// resolve to the DEFINING module — the cross-file join follows the `__init__` re-export. Before the
/// transitive follow, the caller's use resolved only to the `__init__` re-import (a non-call site),
/// so `find_callers` reported `resolved: false`.
#[test]
fn python_resolves_cross_file_through_package_reexport() {
    let query = "class QuerySet:\n    def all(self):\n        return self\n";
    let init = "from pkg.query import QuerySet\n";
    let app = "from pkg import QuerySet\n\n\ndef use():\n    return QuerySet()\n";
    let (_d, store) = scan_repo(&[("pkg/query.py", query), ("pkg/__init__.py", init), ("app.py", app)]);
    if store.lookup("pkg/query.py").is_none() {
        eprintln!("python grammar unavailable — skipping");
        return;
    }
    let query_rel = RelPath::from("pkg/query.py");
    let qs_def = (query.find("class QuerySet").unwrap() + "class ".len()) as u32;
    let uses = basemind::query::resolved_references(&store, &query_rel, qs_def);
    assert!(
        uses.iter().any(|(p, _)| p.as_str() == Some("app.py")),
        "QuerySet must resolve to the package-importing caller app.py via the __init__ re-export; got {uses:?}"
    );
}

/// A Java class imported across packages (`import app.Lib;`) resolves cross-file to the importer's
/// references. (Method-level resolution — a qualified `Lib.greet()` call — is NOT yet covered by the
/// stack-graph engine: methods are not exported, so `find_callers` on a method falls back to the
/// name-based scan. That is a known coverage gap, distinct from this class-level path.)
#[test]
fn java_resolves_cross_file_class_via_import() {
    let lib = "package app;\npublic class Lib {\n  public static int greet() { return 1; }\n}\n";
    let main = "package other;\nimport app.Lib;\npublic class Main {\n  int run() { return Lib.greet(); }\n}\n";
    let (_d, store) = scan_repo(&[("app/Lib.java", lib), ("other/Main.java", main)]);
    if store.lookup("app/Lib.java").is_none() {
        eprintln!("java grammar unavailable — skipping");
        return;
    }
    let lib_rel = RelPath::from("app/Lib.java");
    let class_def = (lib.find("class Lib").unwrap() + "class ".len()) as u32;
    let uses = basemind::query::resolved_references(&store, &lib_rel, class_def);
    assert!(
        uses.iter().any(|(p, _)| p.as_str() == Some("other/Main.java")),
        "the Java class `Lib` must resolve cross-file to the importing Main.java; got {uses:?}"
    );
}
