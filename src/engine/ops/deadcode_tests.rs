//! `code deadcode`: entry conventions, file reachability tiers, symbol judgement and import tiers
//! over synthetic inputs, and the whole report over a scanned Rust crate.

use super::*;
use crate::engine::ops::deadcode_files::{FileFacts, Judged, conventional_entry, is_test_data, unreachable};
use crate::engine::ops::deadcode_symbols::{
    Container, Local, attributes_for_test, container_for_test, entry_name, facts_of,
};
use crate::engine::ops::graph_report::tests::{I, T, graph_of};
use crate::extract::SymbolKind;

#[test]
fn entry_conventions_follow_each_language() {
    let none = FileFacts::default();
    let main = FileFacts {
        go_package: Some("main".into()),
        ..FileFacts::default()
    };
    assert!(conventional_entry("pkg/x_test.go", "go", &none).is_some());
    assert!(conventional_entry("cmd/tool/x.go", "go", &main).is_some());
    assert!(conventional_entry("pkg/x.go", "go", &none).is_none());
    assert!(conventional_entry("app/__main__.py", "python", &none).is_some());
    assert!(conventional_entry("app/test_api.py", "python", &none).is_some());
    let guard = facts_of(
        "python",
        b"def run():\n    pass\nif __name__ == \"__main__\":\n    run()\n",
    );
    assert!(conventional_entry("app/tool.py", "python", &guard).is_some());
    assert!(conventional_entry("app/tool.py", "python", &none).is_none());
    assert!(conventional_entry("web/src/a.test.ts", "typescript", &none).is_some());
    assert!(conventional_entry("web/vite.config.ts", "typescript", &none).is_some());
    assert!(conventional_entry("web/app/users/page.tsx", "tsx", &none).is_some());
    assert!(conventional_entry("web/src/util.ts", "typescript", &none).is_none());
    assert!(conventional_entry("tests/fixtures/repo/src/x.rs", "rust", &none).is_some());
    assert!(conventional_entry("src/x.rs", "rust", &none).is_none());
    let java = facts_of("java", b"class A { public static void main(String[] a) {} }");
    assert!(conventional_entry("src/main/java/A.java", "java", &java).is_some());
    assert_eq!(
        facts_of("go", b"// c\npackage   main // x\n").go_package.as_deref(),
        Some("main")
    );
    assert!(conventional_entry(".pi/extensions/plugin.ts", "typescript", &none).is_some());
    assert!(is_test_data(&RelPath::from("tests/fixtures/http/main.rs")));
    assert!(!is_test_data(&RelPath::from("src/fixtures.rs")));
}

#[test]
fn a_file_no_entry_reaches_is_tiered_by_its_importers_and_language() {
    // main -> used; island_a -> island_b (no entry reaches either); typed reached by type only.
    let graph = graph_of(
        &[
            ("main.rs", "used.rs", I),
            ("island_a.rs", "island_b.rs", I),
            ("main.rs", "typed.rs", T),
        ],
        "rust",
        true,
    );
    let judged = |path: &'static str, entry: Option<&'static str>| (RelPath::from(path), entry);
    let rows = [
        judged("main.rs", Some("a manifest target")),
        judged("used.rs", None),
        judged("island_a.rs", None),
        judged("island_b.rs", None),
        judged("typed.rs", None),
        judged("orphan.rs", None),
    ];
    let files: Vec<Judged<'_>> = rows
        .iter()
        .map(|(path, entry)| Judged {
            path,
            language: "rust",
            entry: *entry,
        })
        .collect();
    let found: Vec<(String, Tier)> = unreachable(&graph, &files)
        .into_iter()
        .map(|file| (file.path.to_string(), file.tier))
        .collect();
    assert_eq!(
        found,
        [
            ("island_a.rs".to_string(), Tier::High),
            ("island_b.rs".to_string(), Tier::Medium),
            ("typed.rs".to_string(), Tier::Low),
            ("orphan.rs".to_string(), Tier::High),
        ]
    );
    let python: Vec<Judged<'_>> = files
        .iter()
        .map(|file| Judged {
            language: "python",
            ..*file
        })
        .collect();
    let orphan = unreachable(&graph, &python)
        .into_iter()
        .find(|file| file.path == RelPath::from("orphan.rs"))
        .expect("orphan");
    assert_eq!(orphan.tier, Tier::Medium, "Python loads modules by name: {orphan:?}");
    assert!(orphan.reason.contains("importlib"), "{}", orphan.reason);
}

#[test]
fn rust_attributes_are_read_above_the_item_across_lines_and_comments() {
    let src = b"use x;\n\n#[derive(Debug)]\n/// doc\n#[tool(\n    name = \"a\",\n)]\npub fn f() {}\n";
    let start = src.windows(6).position(|w| w == b"pub fn").unwrap();
    assert_eq!(attributes_for_test(src, start), ["tool", "derive"]);
    let inline = b"#[test] fn t() {}\n";
    assert_eq!(attributes_for_test(inline, 8), ["test"]);
    let plain = b"fn a() {}\nfn b() {}\n";
    assert!(attributes_for_test(plain, 10).is_empty());
}

#[test]
fn names_a_runner_calls_are_entry_points() {
    use SymbolKind::{Class, Function, Method};
    assert!(entry_name("src/a.rs", "rust", "main", Function));
    assert!(entry_name("pkg/a_test.go", "go", "TestServe", Function));
    assert!(!entry_name("pkg/a.go", "go", "TestServe", Function));
    assert!(entry_name("app/m.py", "python", "__repr__", Method));
    assert!(entry_name("app/m.py", "python", "_fields_", Function));
    assert!(entry_name("tests/test_api.py", "python", "ApiChecks", Class));
    assert!(entry_name("tests/test_api.py", "python", "setUpClass", Method));
    assert!(!entry_name("app/api.py", "python", "ApiChecks", Class));
    assert!(!entry_name("app/api.py", "python", "_private", Function));
}

#[test]
fn the_rust_block_around_an_item_is_read_from_the_text() {
    let src = "impl<'a> BitOr for &'a dyn Flag {\n    fn bitor(self) {}\n}\n\
               fn outer() {\n    struct N;\n    impl Visitor for N {\n        fn visit(&mut self) { if x { y(); } }\n        fn finish(self) {}\n    }\n}\n\
               pub trait Shape {\n    fn area(&self) -> f64 { 0.0 }\n}\n\
               impl Plain {\n    fn own(&self) {}\n}\n\
               fn top() {}\n";
    let at = |needle: &str| src.find(needle).unwrap();
    let bytes = src.as_bytes();
    assert_eq!(container_for_test(bytes, at("fn bitor")), Container::TraitImpl);
    assert_eq!(
        container_for_test(bytes, at("fn finish")),
        Container::TraitImpl,
        "an impl inside a function"
    );
    assert_eq!(container_for_test(bytes, at("fn area")), Container::Trait);
    assert_eq!(container_for_test(bytes, at("fn own")), Container::None);
    assert_eq!(container_for_test(bytes, at("fn top")), Container::None);
}

fn candidate(language: &str, name: &str, kind: SymbolKind, signature: &str) -> Candidate {
    Candidate {
        path: RelPath::from("src/x"),
        language: language.into(),
        name: name.to_string(),
        kind,
        start_byte: 0,
        line: 3,
        signature: Some(signature.to_string()),
        decorator: None,
    }
}

/// The tier of `c` with its name spelled `[code, strings, comments]` times besides its definition.
fn tier_of(c: &Candidate, local: &Local, others: [i64; 3], surface: bool) -> Option<Tier> {
    let in_surface = move |_: &RelPath, _: &str| surface;
    let j = Judge {
        surface: &in_surface,
        others: &[others],
    };
    judge(c, local, None, 0, &j).map(|found| found.tier)
}

#[test]
fn a_symbol_is_tiered_by_its_name_evidence_and_its_language() {
    let plain = Local::default();
    let private = candidate("rust", "helper", SymbolKind::Function, "fn helper()");
    assert_eq!(tier_of(&private, &plain, [0, 0, 0], true), Some(Tier::High));
    assert_eq!(
        tier_of(&private, &plain, [0, 0, 2], true),
        Some(Tier::Medium),
        "comments only"
    );
    assert_eq!(
        tier_of(&private, &plain, [0, 1, 2], true),
        Some(Tier::Low),
        "a string may look it up"
    );
    assert_eq!(
        tier_of(&private, &plain, [1, 1, 2], true),
        None,
        "spelled in code: used"
    );

    let public = candidate("rust", "api", SymbolKind::Function, "pub fn api()");
    assert_eq!(
        tier_of(&public, &plain, [0, 0, 0], true),
        None,
        "exported from a library"
    );
    assert_eq!(
        tier_of(&public, &plain, [0, 0, 0], false),
        Some(Tier::High),
        "a binary's pub"
    );
    let crate_only = candidate("rust", "inner", SymbolKind::Function, "pub(crate) fn inner()");
    assert_eq!(tier_of(&crate_only, &plain, [0, 0, 0], true), Some(Tier::High));

    let test = Local {
        attributes: vec!["test".into()],
        ..Local::default()
    };
    assert_eq!(
        tier_of(&private, &test, [0, 0, 0], true),
        None,
        "a test is an entry point"
    );
    let macro_attr = Local {
        attributes: vec!["derive".into(), "tool".into()],
        ..Local::default()
    };
    assert_eq!(tier_of(&private, &macro_attr, [0, 0, 0], true), Some(Tier::Low));
    let in_trait = Local {
        container: Container::Trait,
        ..Local::default()
    };
    assert_eq!(tier_of(&private, &in_trait, [0, 0, 0], true), Some(Tier::Low));
    let in_trait_impl = Local {
        container: Container::TraitImpl,
        ..Local::default()
    };
    assert_eq!(
        tier_of(&private, &in_trait_impl, [0, 0, 0], true),
        None,
        "the trait requires it"
    );

    let go_exported = candidate("go", "Serve", SymbolKind::Function, "func Serve()");
    assert_eq!(tier_of(&go_exported, &plain, [0, 0, 0], true), None);
    let go_method = candidate("go", "close", SymbolKind::Method, "func (s *S) close()");
    assert_eq!(tier_of(&go_method, &plain, [0, 0, 0], true), Some(Tier::Low));

    let exported = Local {
        exported: true,
        ..Local::default()
    };
    let js = candidate("typescript", "util", SymbolKind::Function, "function util()");
    let default = Local {
        exported: true,
        default_export: true,
        ..Local::default()
    };
    assert_eq!(
        tier_of(&js, &default, [0, 0, 0], false),
        None,
        "a default export is bound by any name"
    );
    assert_eq!(
        tier_of(&js, &exported, [0, 0, 0], true),
        None,
        "a package's public export"
    );
    assert_eq!(tier_of(&js, &exported, [0, 0, 0], false), Some(Tier::Medium));
    assert_eq!(tier_of(&js, &plain, [0, 0, 0], false), Some(Tier::High));

    let py_public = candidate("python", "load", SymbolKind::Function, "def load()");
    assert_eq!(tier_of(&py_public, &plain, [0, 0, 0], true), Some(Tier::Low));
    let py_private = candidate("python", "_load", SymbolKind::Function, "def _load()");
    assert_eq!(tier_of(&py_private, &plain, [0, 0, 0], true), Some(Tier::High));
    let listed = Local {
        in_all: Some(true),
        ..Local::default()
    };
    assert_eq!(
        tier_of(&py_public, &listed, [0, 0, 0], true),
        None,
        "named in `__all__`"
    );
    let unlisted = Local {
        in_all: Some(false),
        ..Local::default()
    };
    assert_eq!(tier_of(&py_public, &unlisted, [0, 0, 0], true), Some(Tier::High));

    let java = candidate("java", "run", SymbolKind::Method, "public void run()");
    assert_eq!(tier_of(&java, &plain, [0, 0, 0], true), None);
}

#[test]
fn unresolved_imports_are_tiered_by_what_the_resolver_sees() {
    assert_eq!(import_tier("typescript", "./gone").0, Tier::High);
    assert_eq!(import_tier("typescript", "@app/gone").0, Tier::Medium);
    assert_eq!(import_tier("python", ".gone").0, Tier::Medium);
    assert_eq!(import_tier("rust", "crate::gone").0, Tier::Low);
}

#[test]
fn a_relative_import_of_a_file_on_disk_is_not_broken() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("web/src")).unwrap();
    std::fs::write(dir.path().join("web/src/a.module.css"), "").unwrap();
    let importer = RelPath::from("web/src/page.tsx");
    assert!(names_unindexed_file(dir.path(), &importer, "./a.module.css"));
    assert!(names_unindexed_file(dir.path(), &importer, "../src/a.module.css"));
    assert!(!names_unindexed_file(dir.path(), &importer, "./gone.css"));
    assert!(
        !names_unindexed_file(dir.path(), &importer, "../../../../etc/hosts"),
        "never outside the root"
    );
    assert!(!names_unindexed_file(dir.path(), &importer, "react"));
}

#[test]
fn a_go_package_outside_internal_is_at_most_low() {
    let graph = graph_of(&[("cmd/main.go", "internal/used/u.go", I)], "go", true);
    let rows = [RelPath::from("pkg/lib/lib.go"), RelPath::from("internal/dead/d.go")];
    let files: Vec<Judged<'_>> = rows
        .iter()
        .map(|path| Judged {
            path,
            language: "go",
            entry: None,
        })
        .collect();
    let found: Vec<(String, Tier)> = unreachable(&graph, &files)
        .into_iter()
        .map(|file| (file.path.to_string(), file.tier))
        .collect();
    assert_eq!(
        found,
        [
            ("pkg/lib/lib.go".to_string(), Tier::Low),
            ("internal/dead/d.go".to_string(), Tier::High)
        ]
    );
}

#[test]
fn a_malformed_request_is_refused() {
    let refused = |p: DeadcodeParams| match Request::parse(p) {
        Err(EngineError::InvalidArgument(message)) => message,
        Err(other) => panic!("refused as invalid, got {other:?}"),
        Ok(_) => panic!("refused, got a request"),
    };
    let kind = refused(DeadcodeParams {
        kind: Some("functions".into()),
        ..DeadcodeParams::default()
    });
    assert!(kind.contains("`files`, `symbols` or `imports`"), "{kind}");
    let tier = refused(DeadcodeParams {
        tier: Some("certain".into()),
        ..DeadcodeParams::default()
    });
    assert!(tier.contains("`high`, `medium` or `low`"), "{tier}");
    let limit = refused(DeadcodeParams {
        limit: Some(0),
        ..DeadcodeParams::default()
    });
    assert!(limit.contains("1 to"), "{limit}");
    let scoped = Request::parse(DeadcodeParams {
        path: Some("src/".into()),
        ..DeadcodeParams::default()
    })
    .expect("request");
    assert!(scoped.in_scope(&RelPath::from("src/a.rs")));
    assert!(!scoped.in_scope(&RelPath::from("srcx/a.rs")));
}

#[test]
fn a_partial_graph_caps_file_findings_at_low() {
    let finding = || DeadFile {
        path: RelPath::from("src/a.rs"),
        language: "rust".into(),
        tier: Tier::High,
        reason: "no file imports it".into(),
    };
    let mut complete = [finding()];
    cap_partial_graph(&mut complete, true);
    assert_eq!(complete[0], finding());
    let mut partial = [finding()];
    cap_partial_graph(&mut partial, false);
    assert_eq!(partial[0].tier, Tier::Low);
    assert!(
        partial[0].reason.contains("not covered the whole index"),
        "{}",
        partial[0].reason
    );
}

#[test]
fn configured_entry_points_match_by_glob_and_bare_name() {
    let paths = [
        RelPath::from("tools/gen.py"),
        RelPath::from("plugins/a/load.ts"),
        RelPath::from("src/lib.rs"),
    ];
    let none = configured_entries(&[], paths.iter()).expect("no globs");
    assert!(none.is_empty());
    let globs = ["tools/*.py".to_string(), "plugins".to_string()];
    let mut found: Vec<String> = configured_entries(&globs, paths.iter())
        .expect("globs")
        .into_iter()
        .map(ToString::to_string)
        .collect();
    found.sort();
    assert_eq!(found, ["plugins/a/load.ts", "tools/gen.py"]);
    let bad = configured_entries(&["a[".to_string()], paths.iter());
    assert!(matches!(bad, Err(EngineError::InvalidArgument(_))));
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
mod scanned {
    use super::*;
    use crate::engine::ops::graph_report::tests::{scanned_workspace, write};

    /// A library crate: `lib.rs` declares `used`; `orphan.rs` is declared by nothing; `used.rs`
    /// holds a private function nothing names, one only named in a comment, a test, an attribute
    /// macro's target and a trait impl.
    fn crate_with_dead_code(root: &std::path::Path) {
        write(root, "basemind.toml", "");
        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
        );
        write(
            root,
            "src/lib.rs",
            "mod used;\npub use used::api;\nuse missing_crate::gone;\n",
        );
        write(
            root,
            "src/used.rs",
            "pub fn api() { live(); }\n\
             fn live() {}\n\
             fn never_named() {}\n\
             // spelled_only is mentioned here only\n\
             fn spelled_only() {}\n\
             #[tool]\n\
             fn registered() {}\n\
             #[test]\n\
             fn checks() {}\n\
             fn by_name() {}\n\
             pub fn table() -> &'static str { \"by_name\" }\n\
             struct Shown;\n\
             impl std::fmt::Display for Shown {\n    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { Ok(()) }\n}\n",
        );
        write(root, "src/orphan.rs", "pub fn lost() {}\n");
    }

    fn report(ws: &Workspace, p: DeadcodeParams) -> DeadcodeResponse {
        ws.code_deadcode(p).expect("deadcode")
    }

    #[test]
    fn a_scanned_crate_reports_each_kind_with_its_tier() {
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        crate_with_dead_code(dir.path());
        let ws = scanned_workspace(dir.path(), false);
        let all = report(&ws, DeadcodeParams::default());

        let files: Vec<(String, Tier)> = all
            .files
            .as_ref()
            .expect("files")
            .iter()
            .map(|f| (f.path.to_string(), f.tier))
            .collect();
        assert_eq!(files, [("src/orphan.rs".to_string(), Tier::High)], "{all:#?}");

        let symbols: Vec<(String, Tier)> = all
            .symbols
            .as_ref()
            .expect("symbols")
            .iter()
            .map(|s| (s.name.clone(), s.tier))
            .collect();
        assert!(
            symbols.contains(&("never_named".to_string(), Tier::High)),
            "{symbols:?}"
        );
        assert!(
            symbols.contains(&("spelled_only".to_string(), Tier::Medium)),
            "{symbols:?}"
        );
        assert!(symbols.contains(&("registered".to_string(), Tier::Low)), "{symbols:?}");
        assert!(symbols.contains(&("by_name".to_string(), Tier::Low)), "{symbols:?}");
        for used in ["api", "live", "checks", "fmt", "lost"] {
            assert!(
                !symbols.iter().any(|(name, _)| name == used),
                "{used} is used, exported, a test or a trait impl member: {symbols:?}"
            );
        }
        let line = all
            .symbols
            .as_ref()
            .unwrap()
            .iter()
            .find(|s| s.name == "never_named")
            .unwrap()
            .line;
        assert_eq!(line, 3);

        let imports = all.imports.as_ref().expect("imports");
        assert!(
            imports
                .iter()
                .any(|i| i.path == RelPath::from("src/lib.rs") && i.specifier.contains("missing_crate")),
            "{imports:?}"
        );
        assert!(
            all.coverage
                .iter()
                .any(|c| c.language == "rust" && c.analysed == ["files", "symbols", "imports"])
        );
        assert!(all.entry_points >= 1);

        let high = report(
            &ws,
            DeadcodeParams {
                tier: Some("high".into()),
                kind: Some("symbols".into()),
                ..DeadcodeParams::default()
            },
        );
        assert!(high.files.is_none() && high.imports.is_none());
        let names: Vec<&str> = high.symbols.as_ref().unwrap().iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"never_named") && !names.contains(&"spelled_only"),
            "{names:?}"
        );
        assert_eq!(high.counts["symbols"].medium, 0);

        let scoped = report(
            &ws,
            DeadcodeParams {
                path: Some("src/orphan.rs".into()),
                ..DeadcodeParams::default()
            },
        );
        assert_eq!(scoped.files.as_ref().unwrap().len(), 1);
        assert!(scoped.symbols.as_ref().unwrap().is_empty(), "{:?}", scoped.symbols);

        let one = report(
            &ws,
            DeadcodeParams {
                kind: Some("symbols".into()),
                limit: Some(1),
                ..DeadcodeParams::default()
            },
        );
        assert_eq!(one.symbols.as_ref().unwrap().len(), 1);
        assert_eq!(one.symbols.as_ref().unwrap()[0].tier, Tier::High, "most sure first");
        assert!(one.omitted["symbols"] >= 2);
    }
}
