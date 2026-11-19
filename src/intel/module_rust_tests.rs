//! Tests of Rust file-level module resolution.

use super::*;
use crate::intel::resolver::ProbeSet;

fn paths(raw: &str) -> Vec<String> {
    use_paths(raw).into_iter().map(|path| path.join("::")).collect()
}

#[test]
fn use_trees_expand_to_leaf_paths() {
    assert_eq!(paths("use crate::a::B;"), ["crate::a::B"]);
    assert_eq!(
        paths("pub(crate) use crate::a::{self, b::C as D, e::*, /* x */ f};"),
        ["crate::a", "crate::a::b::C", "crate::a::e", "crate::a::f"]
    );
    assert_eq!(paths("use super::*;"), ["super"]);
    assert_eq!(paths("use ::std::fmt;"), ["::std::fmt"]);
    assert_eq!(
        paths("use std::{fmt, io::{self, Read}};"),
        ["std::fmt", "std::io", "std::io::Read"]
    );
    assert_eq!(paths("pub use r#async::run;"), ["async::run"]);
    assert_eq!(paths("use a::b as _;"), ["a::b"]);
    assert!(paths("extern crate foo;").is_empty());
}

#[test]
fn path_attribute_reads_the_attribute_above_a_module() {
    let src = b"#[cfg(unix)]\n#[path = \"sys/unix.rs\"]\n// the platform layer\nmod sys;\n";
    let at = src.windows(4).position(|w| w == b"mod ").unwrap();
    assert_eq!(path_attribute(src, at).as_deref(), Some("sys/unix.rs"));
    let plain = b"fn f() {}\nmod a;\n";
    let at = plain.windows(4).position(|w| w == b"mod ").unwrap();
    assert_eq!(path_attribute(plain, at), None);
    assert_eq!(normalize("a/./b/../c.rs").as_deref(), Some("a/c.rs"));
    assert_eq!(normalize("../x.rs"), None);
}

#[test]
fn module_paths_follow_the_crate_layout() {
    let layout = CrateLayout::new(
        "core/src".to_string(),
        vec!["core/src/lib.rs".to_string(), "core/src/main.rs".to_string()],
    );
    assert_eq!(module_path(&layout, "core/src/lib.rs"), Some(vec![]));
    assert_eq!(module_path(&layout, "core/src/main.rs"), Some(vec![]));
    assert_eq!(module_path(&layout, "core/src/a/mod.rs"), Some(vec!["a".to_string()]));
    assert_eq!(
        module_path(&layout, "core/src/a/b.rs"),
        Some(vec!["a".to_string(), "b".to_string()])
    );
    assert_eq!(module_path(&layout, "other/x.rs"), None);
}

fn symbol(name: &str, kind: SymbolKind, start: usize, end: usize) -> Symbol {
    Symbol {
        name: name.to_string(),
        kind,
        start_byte: start as u32,
        end_byte: end as u32,
        start_row: 0,
        start_col: 0,
        signature: None,
        decorators: Vec::new(),
    }
}

/// An outline of `source` with its `mod` items and `use`/`extern crate` declarations, found by
/// line, as the L1 extractor records them.
fn outline(source: &str) -> (Vec<Symbol>, Vec<Import>) {
    let mut symbols = Vec::new();
    let mut imports = Vec::new();
    let mut offset = 0;
    let mut open: Vec<(String, usize)> = Vec::new();
    for line in source.split_inclusive('\n') {
        let text = line.trim();
        let start = offset + line.find(text).unwrap_or(0);
        if let Some(rest) = text.strip_prefix("pub ").unwrap_or(text).strip_prefix("mod ") {
            let name = rest.trim_end_matches([';', '{', ' ', '}']).to_string();
            if text.ends_with([';', '}']) {
                symbols.push(symbol(&name, SymbolKind::Module, start, start + text.len()));
            } else {
                open.push((name, start));
            }
        } else if text == "}" {
            let (name, start) = open.pop().unwrap();
            symbols.push(symbol(
                &name,
                SymbolKind::Module,
                start,
                offset + line.find('}').unwrap() + 1,
            ));
        } else if text.starts_with("use ") || text.starts_with("pub use ") {
            imports.push(Import {
                module: None,
                raw: text.to_string(),
                start_byte: start as u32,
                end_byte: (start + text.len()) as u32,
            });
        } else if let Some(rest) = text.strip_prefix("extern crate ") {
            imports.push(Import {
                module: Some(rest.trim_end_matches(';').split(' ').next().unwrap().to_string()),
                raw: text.to_string(),
                start_byte: start as u32,
                end_byte: (start + text.len()) as u32,
            });
        }
        offset += line.len();
    }
    (symbols, imports)
}

/// Resolve `importer` in a repository holding `files` (all indexed) on disk.
fn resolve(files: &[(&str, &str)], importer: &str) -> (Vec<(String, ModuleTarget)>, ProbeSet) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, body) in files {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    let indexed: AHashSet<String> = files
        .iter()
        .filter(|(rel, _)| rel.ends_with(".rs"))
        .map(|(rel, _)| (*rel).to_string())
        .collect();
    let lookup = |rel: &str| indexed.contains(rel);
    let source = files.iter().find(|(rel, _)| *rel == importer).unwrap().1;
    let mut probes = ProbeSet::default();
    let (symbols, imports) = outline(source);
    let content_hash = crate::hashing::hex(&crate::hashing::hash_bytes(source.as_bytes()));
    let imports = RustModules::default().resolve(
        &mut Probe {
            root: dir.path(),
            indexed: &lookup,
            probes: &mut probes,
        },
        importer,
        &content_hash,
        &symbols,
        &imports,
    );
    let out = imports
        .into_iter()
        .map(|import| (import.specifier, import.target))
        .collect();
    (out, probes)
}

fn files(rels: &[&str]) -> ModuleTarget {
    ModuleTarget::Files(rels.iter().map(|rel| (*rel).to_string()).collect())
}

const MANIFEST: &str = "[package]\nname = \"demo\"\n";

#[test]
fn mod_declarations_load_sibling_and_nested_module_files() {
    let lib = "mod a;\npub mod b;\nmod gone;\n#[path = \"sys/unix.rs\"]\nmod sys;\nmod inline {\n    mod deep;\n}\n";
    let (resolved, probes) = resolve(
        &[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", lib),
            ("src/a.rs", "mod c;\n"),
            ("src/a/c.rs", ""),
            ("src/b/mod.rs", ""),
            ("src/sys/unix.rs", ""),
            ("src/inline/deep.rs", ""),
        ],
        "src/lib.rs",
    );
    assert_eq!(
        resolved,
        [
            ("mod a".to_string(), files(&["src/a.rs"])),
            ("mod b".to_string(), files(&["src/b/mod.rs"])),
            ("mod gone".to_string(), ModuleTarget::Unresolved),
            ("mod sys".to_string(), files(&["src/sys/unix.rs"])),
            ("mod deep".to_string(), files(&["src/inline/deep.rs"])),
        ]
    );
    assert!(probes.contains("src/gone.rs") && probes.contains("src/gone/mod.rs"));
    assert!(probes.contains("src/Cargo.toml") && probes.contains("Cargo.toml"));

    let (resolved, _) = resolve(
        &[("Cargo.toml", MANIFEST), ("src/a.rs", "mod c;\n"), ("src/a/c.rs", "")],
        "src/a.rs",
    );
    assert_eq!(resolved, [("mod c".to_string(), files(&["src/a/c.rs"]))]);
}

#[test]
fn use_paths_resolve_to_the_longest_module_file() {
    let b = "use crate::a::Thing;\nuse crate::a::c::{self, Deep};\nuse crate::Root;\nuse super::sibling::f;\n\
             use self::inner::g;\nuse std::fmt;\nuse serde::Serialize;\nextern crate alloc;\nmod inner;\n\
             mod tests {\n    use super::*;\n    use super::super::a::Thing;\n}\n";
    let (resolved, _) = resolve(
        &[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", ""),
            ("src/a.rs", ""),
            ("src/a/c.rs", ""),
            ("src/m/mod.rs", ""),
            ("src/m/b.rs", b),
            ("src/m/sibling.rs", ""),
            ("src/m/b/inner.rs", ""),
        ],
        "src/m/b.rs",
    );
    assert_eq!(
        resolved,
        [
            ("crate::a::Thing".to_string(), files(&["src/a.rs"])),
            ("crate::a::c".to_string(), files(&["src/a/c.rs"])),
            ("crate::a::c::Deep".to_string(), files(&["src/a/c.rs"])),
            ("crate::Root".to_string(), files(&["src/lib.rs"])),
            ("super::sibling::f".to_string(), files(&["src/m/sibling.rs"])),
            ("self::inner::g".to_string(), files(&["src/m/b/inner.rs"])),
            ("std::fmt".to_string(), ModuleTarget::External),
            ("serde::Serialize".to_string(), ModuleTarget::External),
            ("extern crate alloc".to_string(), ModuleTarget::External),
            ("mod inner".to_string(), files(&["src/m/b/inner.rs"])),
            ("super".to_string(), ModuleTarget::SelfFile),
            ("super::super::a::Thing".to_string(), files(&["src/m/mod.rs"])),
        ]
    );
}

#[test]
fn crate_roots_cover_binaries_tests_and_a_missing_manifest() {
    let (resolved, _) = resolve(
        &[
            ("tool/Cargo.toml", MANIFEST),
            ("tool/src/bin/cli.rs", "mod args;\nuse crate::args::Args;\n"),
            ("tool/src/bin/args.rs", ""),
            ("tool/tests/it.rs", "mod common;\n"),
            ("tool/tests/common/mod.rs", ""),
        ],
        "tool/src/bin/cli.rs",
    );
    assert_eq!(
        resolved,
        [
            ("mod args".to_string(), files(&["tool/src/bin/args.rs"])),
            ("crate::args::Args".to_string(), files(&["tool/src/bin/args.rs"])),
        ]
    );
    let (resolved, _) = resolve(
        &[
            ("tool/Cargo.toml", MANIFEST),
            ("tool/tests/it.rs", "mod common;\n"),
            ("tool/tests/common/mod.rs", ""),
        ],
        "tool/tests/it.rs",
    );
    assert_eq!(
        resolved,
        [("mod common".to_string(), files(&["tool/tests/common/mod.rs"]))]
    );

    let (resolved, _) = resolve(
        &[
            ("Cargo.toml", MANIFEST),
            (
                "tests/suite.rs",
                "#[path = \"suite/case.rs\"]\nmod case;\nfn shared() {}\n",
            ),
            ("tests/suite/case.rs", "use super::shared;\n"),
        ],
        "tests/suite/case.rs",
    );
    assert_eq!(resolved, [("super::shared".to_string(), files(&["tests/suite.rs"]))]);

    let (resolved, _) = resolve(&[("loose/x.rs", "use crate::y::Z;\n")], "loose/x.rs");
    assert_eq!(resolved, [("crate::y::Z".to_string(), ModuleTarget::Unresolved)]);
}

#[test]
fn a_lib_or_main_file_that_is_no_crate_root_is_a_module() {
    let (resolved, _) = resolve(
        &[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", "mod foo;\n"),
            ("src/foo.rs", "mod main;\n"),
            ("src/foo/main.rs", "mod helper;\nuse self::helper::X;\n"),
            ("src/foo/main/helper.rs", ""),
            ("src/foo/helper.rs", ""),
        ],
        "src/foo/main.rs",
    );
    assert_eq!(
        resolved,
        [
            ("mod helper".to_string(), files(&["src/foo/main/helper.rs"])),
            ("self::helper::X".to_string(), files(&["src/foo/main/helper.rs"])),
        ]
    );

    let (resolved, _) = resolve(
        &[
            ("Cargo.toml", MANIFEST),
            ("examples/demo/main.rs", "mod lib;\n"),
            (
                "examples/demo/lib.rs",
                "mod inner;\nuse self::inner::Y;\nuse super::Z;\n",
            ),
            ("examples/demo/lib/inner.rs", ""),
            ("examples/demo/inner.rs", ""),
        ],
        "examples/demo/lib.rs",
    );
    assert_eq!(
        resolved,
        [
            ("mod inner".to_string(), files(&["examples/demo/lib/inner.rs"])),
            ("self::inner::Y".to_string(), files(&["examples/demo/lib/inner.rs"])),
            ("super::Z".to_string(), files(&["examples/demo/main.rs"])),
        ]
    );

    // Without a `Cargo.toml` there is no layout, so the name is all there is to go on.
    let (resolved, _) = resolve(&[("loose/main.rs", "mod a;\n"), ("loose/a.rs", "")], "loose/main.rs");
    assert_eq!(resolved, [("mod a".to_string(), files(&["loose/a.rs"]))]);
}

#[test]
fn cargo_toml_targets_are_crate_roots() {
    let manifest = "[package]\nname = \"demo\"\n\n[lib]\npath = \"core/lib.rs\"\n\n\
                    [[bin]]\nname = \"gen\"\npath = \"tools/gen/main.rs\"\n\n\
                    [[bin]]\nname = \"tool\"\npath = \"src/tool/main.rs\"\n";
    let repo = [
        ("Cargo.toml", manifest),
        ("core/lib.rs", "mod a;\nmod b;\n"),
        ("core/a.rs", "use crate::b::B;\nuse crate::Root;\n"),
        ("core/b.rs", ""),
        ("tools/gen/main.rs", "mod opts;\nuse crate::opts::O;\n"),
        ("tools/gen/opts.rs", "use crate::Run;\n"),
        ("src/lib.rs", ""),
        ("src/tool/main.rs", "mod args;\n"),
        ("src/tool/args.rs", "use crate::Root;\n"),
    ];
    let (resolved, _) = resolve(&repo, "core/a.rs");
    assert_eq!(
        resolved,
        [
            ("crate::b::B".to_string(), files(&["core/b.rs"])),
            ("crate::Root".to_string(), files(&["core/lib.rs"])),
        ]
    );
    let (resolved, _) = resolve(&repo, "tools/gen/main.rs");
    assert_eq!(
        resolved,
        [
            ("mod opts".to_string(), files(&["tools/gen/opts.rs"])),
            ("crate::opts::O".to_string(), files(&["tools/gen/opts.rs"])),
        ]
    );
    let (resolved, _) = resolve(&repo, "tools/gen/opts.rs");
    assert_eq!(resolved, [("crate::Run".to_string(), files(&["tools/gen/main.rs"]))]);
    let (resolved, _) = resolve(&repo, "src/tool/main.rs");
    assert_eq!(resolved, [("mod args".to_string(), files(&["src/tool/args.rs"]))]);
    let (resolved, _) = resolve(&repo, "src/tool/args.rs");
    assert_eq!(resolved, [("crate::Root".to_string(), files(&["src/tool/main.rs"]))]);
}

#[test]
fn a_shared_test_module_resolves_against_every_test_crate_including_it() {
    let common = "mod fixtures;\nuse crate::common::fixtures::F;\nuse crate::Shared;\nuse super::Shared as S;\n";
    let repo = [
        ("Cargo.toml", MANIFEST),
        ("tests/b.rs", "mod common;\n"),
        ("tests/a.rs", "mod common;\nstruct Shared;\n"),
        ("tests/c.rs", "// mod common;\nmod commons;\n"),
        ("tests/d.rs", "mod common {\n}\n"),
        ("tests/common/mod.rs", common),
        ("tests/common/fixtures.rs", "use crate::common::F;\n"),
        ("tests/x/main.rs", "mod util;\n"),
        ("tests/x/util.rs", "use crate::Root;\n"),
    ];
    let (resolved, probes) = resolve(&repo, "tests/common/mod.rs");
    assert_eq!(
        resolved,
        [
            ("mod fixtures".to_string(), files(&["tests/common/fixtures.rs"])),
            (
                "crate::common::fixtures::F".to_string(),
                files(&["tests/common/fixtures.rs"])
            ),
            ("crate::Shared".to_string(), files(&["tests/a.rs", "tests/b.rs"])),
            ("super::Shared".to_string(), files(&["tests/a.rs", "tests/b.rs"])),
        ]
    );
    assert!(
        probes.contains("tests"),
        "a test crate added to tests/ must re-stitch the module"
    );
    let (resolved, _) = resolve(&repo, "tests/common/fixtures.rs");
    assert_eq!(
        resolved,
        [("crate::common::F".to_string(), files(&["tests/common/mod.rs"]))]
    );
    let (resolved, _) = resolve(&repo, "tests/x/util.rs");
    assert_eq!(resolved, [("crate::Root".to_string(), files(&["tests/x/main.rs"]))]);
}
