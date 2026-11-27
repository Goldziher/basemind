//! `code deadcode`: each language's export and entry rule read from real source text, the module
//! declarations the Rust resolver cannot follow, and the definition count of the text pass.

use super::tests::{candidate, tier_of};
use super::*;
use crate::engine::ops::deadcode_files::{
    CrateModules, FileFacts, PackageCaps, cap_by_package, conventional_entry, crate_modules, named_by_module_string,
};
use crate::engine::ops::deadcode_symbols::{
    Local, Names, defined_for_test, facts_of, hidden_for_test, local_of, python_all,
};
use crate::extract::SymbolKind;

/// The `Local` of the definition that starts at `needle` in a `language` file.
fn local_at(language: &'static str, text: &str, needle: &str, name: &str) -> Local {
    let mut c = candidate(language, SymbolKind::Function, 0);
    c.start_byte = u32::try_from(text.find(needle).expect("needle")).unwrap();
    let all = if language == "python" {
        python_all(text.as_bytes())
    } else {
        None
    };
    local_of(&c, name, text.as_bytes(), all.as_ref())
}

#[test]
fn a_js_export_is_read_from_the_declaration() {
    let text = "export function shown() {}\nexport default function main() {}\nfunction hidden() {}\n\
                export const arrow = () => 1;\nexport async function later() {}\n";
    let shown = local_at("typescript", text, "function shown", "shown");
    assert!(shown.exported && !shown.default_export);
    let default = local_at("typescript", text, "function main", "main");
    assert!(default.exported && default.default_export);
    let hidden = local_at("typescript", text, "function hidden", "hidden");
    assert!(!hidden.exported);
    assert!(local_at("typescript", text, "arrow =", "arrow").exported);
    assert!(local_at("typescript", text, "function later", "later").exported);

    let js = candidate("typescript", SymbolKind::Function, 0);
    assert_eq!(
        tier_of(&js, "shown", &shown, [0, 0, 0], false),
        Some(Tier::Medium),
        "exported outside a package surface"
    );
    assert_eq!(tier_of(&js, "hidden", &hidden, [0, 0, 0], false), Some(Tier::High));
    let in_entry = Local {
        entry_file: true,
        ..shown.clone()
    };
    assert_eq!(
        tier_of(&js, "activate", &in_entry, [0, 0, 0], false),
        None,
        "an export of an entry file is what its loader calls"
    );
}

#[test]
fn an_exported_js_name_is_low_while_the_package_surface_is_unknown() {
    let js = candidate("typescript", SymbolKind::Function, 0);
    let exported = Local {
        exported: true,
        ..Local::default()
    };
    let path = RelPath::from("web/src/util.ts");
    let not_surface = |_: &RelPath, _: &str| false;
    let none = |_: &RelPath| false;
    let all = |_: &RelPath| true;
    let tier = |surface_partial: bool, build_output: &dyn Fn(&RelPath) -> bool| {
        let j = Judge {
            surface: &not_surface,
            others: &[[0, 0, 0]],
            surface_partial,
            build_output,
            unreadable: 0,
        };
        judge(&js, &path, "util", &exported, None, &j).map(|found| (found.tier, found.reason))
    };
    assert_eq!(tier(false, &none).map(|f| f.0), Some(Tier::Medium));
    let partial = tier(true, &none).expect("judged");
    assert_eq!(partial.0, Tier::Low, "{}", partial.1);
    assert!(partial.1.contains("resolve pass"), "{}", partial.1);
    let built = tier(false, &all).expect("judged");
    assert_eq!(built.0, Tier::Low);
    assert!(built.1.contains("build output"), "{}", built.1);
}

#[test]
fn a_python_all_is_parsed_once_from_its_literals() {
    let text = "__all__ = [\"load\", 'save']\n__all__ += (\"extra\",)\n__all__.extend([\"more\"])\n\
                def load(): pass\ndef save(): pass\ndef helper(): pass\ndef _private(): pass\n";
    let all = python_all(text.as_bytes()).expect("a literal `__all__`");
    let mut names: Vec<&str> = all.iter().map(|name| &**name).collect();
    names.sort_unstable();
    assert_eq!(names, ["extra", "load", "more", "save"]);
    assert_eq!(local_at("python", text, "def load", "load").in_all, Some(true));
    let helper = local_at("python", text, "def helper", "helper");
    assert_eq!(helper.in_all, Some(false));
    let py = candidate("python", SymbolKind::Function, 0);
    assert_eq!(
        tier_of(&py, "helper", &helper, [0, 0, 0], true),
        Some(Tier::High),
        "public, but `__all__` leaves it out"
    );
    assert_eq!(
        tier_of(
            &py,
            "load",
            &local_at("python", text, "def load", "load"),
            [0, 0, 0],
            true
        ),
        None
    );
    assert!(
        python_all(b"from base import names\n__all__ = names\n").is_none(),
        "not a literal"
    );
    assert!(python_all(b"def f(): pass\n").is_none());
    let no_all = local_at("python", "def load(): pass\n", "def load", "load");
    assert_eq!(no_all.in_all, None);
    assert_eq!(
        tier_of(&py, "load", &no_all, [0, 0, 0], true),
        Some(Tier::Low),
        "without `__all__`, a public name may be imported from outside"
    );
    let in_entry = Local {
        entry_file: true,
        ..Local::default()
    };
    assert_eq!(
        tier_of(&py, "_hook", &in_entry, [0, 0, 0], true),
        Some(Tier::Low),
        "a loader may call a name of its entry file"
    );
}

#[test]
fn go_exports_by_capital_except_in_package_main_and_cgo_exports() {
    let main = facts_of(
        "go",
        b"package main\n\n//export Callback\nfunc Callback() {}\nfunc Serve() {}\n",
    );
    assert_eq!(main.go_package.as_deref(), Some("main"));
    assert_eq!(main.go_exports, ["Callback"]);
    let lib = facts_of("go", b"// Package x.\npackage x\n");
    let go = candidate("go", SymbolKind::Function, 0);
    let in_package = |facts: &FileFacts, name: &str| {
        let surface = |_: &RelPath, _: &str| false;
        let none = |_: &RelPath| false;
        let j = Judge {
            surface: &surface,
            others: &[[0, 0, 0]],
            surface_partial: false,
            build_output: &none,
            unreadable: 0,
        };
        judge(
            &go,
            &RelPath::from("cmd/x/main.go"),
            name,
            &Local::default(),
            Some(facts),
            &j,
        )
        .map(|f| f.tier)
    };
    assert_eq!(
        in_package(&lib, "Serve"),
        None,
        "a capital name outside `package main` is exported"
    );
    assert_eq!(
        in_package(&main, "Serve"),
        Some(Tier::High),
        "`package main` exports nothing"
    );
    assert_eq!(in_package(&main, "Callback"), None, "cgo `//export` hands it to C");
}

#[test]
fn entry_markers_tolerate_spacing_and_order() {
    for text in [
        "if __name__=='__main__':\n    run()\n",
        "if __name__ == \"__main__\":\n    run()\n",
        "if \"__main__\" == __name__:\n    run()\n",
    ] {
        assert!(facts_of("python", text.as_bytes()).main_guard, "{text}");
    }
    assert!(!facts_of("python", b"name = '__main__'\n").main_guard);
    for text in [
        "class A { static public void main(String[] a) {} }",
        "class A { public static void main(String[] a) {} }",
        "void main() { System.out.println(1); }",
    ] {
        assert!(facts_of("java", text.as_bytes()).java_main, "{text}");
    }
}

#[test]
fn javascript_apps_and_frameworks_have_conventional_entries() {
    let none = FileFacts::default();
    for path in [
        "web/src/main.tsx",
        "src/index.js",
        "web/app/root.tsx",
        "web/app/entry.client.tsx",
        "web/src/middleware.ts",
        "web/src/instrumentation.ts",
        "web/src/hooks.server.ts",
    ] {
        let language = if path.ends_with(".js") {
            "javascript"
        } else {
            "typescript"
        };
        assert!(conventional_entry(path, language, &none).is_some(), "{path}");
    }
    assert!(conventional_entry("web/src/lib/main.ts", "typescript", &none).is_none());
    assert!(conventional_entry("web/src/hooks/useThing.ts", "typescript", &none).is_none());
}

#[test]
fn django_and_celery_modules_and_dotted_strings_are_entries() {
    let none = FileFacts::default();
    for path in [
        "shop/admin.py",
        "shop/apps.py",
        "shop/urls.py",
        "shop/tasks.py",
        "shop/management/commands/load_orders.py",
        "shop/templatetags/money.py",
    ] {
        assert!(conventional_entry(path, "python", &none).is_some(), "{path}");
    }
    assert!(conventional_entry("shop/helpers.py", "python", &none).is_none());
    let settings = facts_of(
        "python",
        b"ROOT_URLCONF = 'site.urls'\nMIDDLEWARE = [\"shop.middleware.Audit\"]\nNAME = 'not a module'\n\
          handler = \"pkg.jobs:run\"\n",
    );
    assert_eq!(
        settings.module_strings,
        ["site.urls", "shop.middleware.Audit", "pkg.jobs"]
    );
    let strings: ahash::AHashSet<&str> = ["site.urls", "shop.middleware", "pkg.jobs"].into_iter().collect();
    assert!(named_by_module_string("backend/site/urls.py", &strings));
    assert!(named_by_module_string("shop/middleware.py", &strings));
    assert!(named_by_module_string("src/pkg/jobs/__init__.py", &strings));
    assert!(!named_by_module_string("shop/urls.py", &strings));
    assert!(
        !named_by_module_string("jobs.py", &strings),
        "one segment is too ambiguous"
    );
}

fn finding(path: &str, language: &str) -> DeadFile {
    DeadFile {
        path: RelPath::from(path),
        language: language.into(),
        tier: Tier::High,
        reason: "no file imports it and it is no entry point".into(),
    }
}

#[test]
fn a_rust_module_declared_where_the_resolver_cannot_see_is_low() {
    let tokio_style = hidden_for_test(
        b"cfg_rt! {\n    pub mod runtime;\n}\ncfg_fs! { pub(crate) mod fs; }\n// mod commented;\n\
          include!(\"generated/table.rs\");\n#[cfg_attr(unix, path = \"sys/unix.rs\")]\nmod sys;\n",
    );
    assert_eq!(
        tokio_style.mod_names,
        ["runtime", "fs", "sys"],
        "a `mod` in a comment declares nothing"
    );
    assert_eq!(tokio_style.rs_strings, ["table.rs", "unix.rs"]);
    assert!(!tokio_style.macro_modules);
    assert!(hidden_for_test(b"macro_rules! m { ($n:ident) => { mod $n; } }\n").macro_modules);

    let crates = vec!["tokio/".to_string(), "other/".to_string()];
    let facts = FileFacts {
        rust: tokio_style,
        ..FileFacts::default()
    };
    let lib = RelPath::from("tokio/src/lib.rs");
    let modules: ahash::AHashMap<String, CrateModules> = crate_modules(&crates, [(&lib, &facts)].into_iter());
    let mut found = [
        finding("tokio/src/runtime.rs", "rust"),
        finding("tokio/src/fs/mod.rs", "rust"),
        finding("tokio/src/generated/table.rs", "rust"),
        finding("tokio/src/sys/unix.rs", "rust"),
        finding("tokio/src/orphan.rs", "rust"),
        finding("other/src/runtime.rs", "rust"),
    ];
    cap_by_package(
        &mut found,
        &PackageCaps {
            crates: &crates,
            crate_modules: &modules,
            python_packages: &[],
            js_build_output: &[],
        },
    );
    let tiers: Vec<(&str, Tier)> = found.iter().map(|f| (f.path.as_str().unwrap(), f.tier)).collect();
    assert_eq!(
        tiers,
        [
            ("tokio/src/runtime.rs", Tier::Low),
            ("tokio/src/fs/mod.rs", Tier::Low),
            ("tokio/src/generated/table.rs", Tier::Low),
            ("tokio/src/sys/unix.rs", Tier::Low),
            ("tokio/src/orphan.rs", Tier::High),
            ("other/src/runtime.rs", Tier::High),
        ],
        "only a name its own crate declares out of sight"
    );
    assert!(found[0].reason.contains("inside a macro"), "{}", found[0].reason);
}

#[test]
fn public_python_modules_and_build_output_packages_are_low() {
    let mut found = [
        finding("pkg/utils.py", "python"),
        finding("pkg/_internal/x.py", "python"),
        finding("pkg/_private.py", "python"),
        finding("scripts/x.py", "python"),
        finding("lib/src/feature.ts", "typescript"),
        finding("app/src/feature.ts", "typescript"),
    ];
    let modules = ahash::AHashMap::new();
    cap_by_package(
        &mut found,
        &PackageCaps {
            crates: &[],
            crate_modules: &modules,
            python_packages: &["pkg/".to_string()],
            js_build_output: &["lib/".to_string()],
        },
    );
    let tiers: Vec<Tier> = found.iter().map(|f| f.tier).collect();
    assert_eq!(
        tiers,
        [Tier::Low, Tier::High, Tier::High, Tier::High, Tier::Low, Tier::High]
    );
}

#[test]
fn a_definition_lexed_outside_code_cancels_no_use() {
    // `/*` inside the template literal once opened a comment that swallowed `helper`'s definition.
    let text = "const g = glob(`src/**/*.ts`);\nfunction helper() {}\nhelper();\n/** doc */\n";
    let l1 = crate::extract::l1::extract_l1("typescript", text.as_bytes()).expect("outline");
    let mut names = Names::default();
    let helper = names.intern("helper");
    assert_eq!(
        defined_for_test("typescript", text.as_bytes(), &l1, &names),
        [helper],
        "the definition token is code, so it is taken out once"
    );
    let commented = "/* function helper() {} */\nfunction helper() {}\n";
    let l1 = crate::extract::l1::extract_l1("typescript", commented.as_bytes()).expect("outline");
    assert_eq!(
        defined_for_test("typescript", commented.as_bytes(), &l1, &names),
        [helper],
        "the commented-out copy is no definition and not taken out"
    );
}
