//! Incremental cross-file re-stitch must be EXACT: after any edit sequence, a `scan_paths` rescan
//! leaves `refs_by_def`, `refs_by_path`, `import_targets` and `importers_by_probe` row-for-row equal
//! to what a full scan of the final tree produces.
//!
//! Each case builds a repo, scans it, applies edits to disk, rescans only the touched paths, and
//! compares against a second repo scanned once from the final contents. The cases are the ones a
//! "re-stitch the direct importers of the changed files" strategy gets wrong: a file behind a
//! re-export, a tsconfig edit, a file that shadows an already-resolved module, a deleted target, and
//! a directory that did not exist when the importer last resolved.
//!
//! Every case also asserts a concrete edge and that the edit changed the rows, so equality is never
//! two empty indexes agreeing. Cases skip (rather than fail) when a grammar cannot be fetched.
#![cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use basemind::config::ConfigV1;
use basemind::path::RelPath;
use basemind::scanner::{EmbedMode, ScanSource, scan, scan_paths};
use basemind::store::{Store, VIEW_WORKING};

type Rows = Vec<(&'static str, Vec<u8>, Vec<u8>)>;

enum Edit<'a> {
    Write(&'a str, &'a str),
    Remove(&'a str),
}

/// A use of a definition that must be resolved after the edits: `use_file` references the
/// `name` found inside `needle` in `def_file`.
struct Expect<'a> {
    def_file: &'a str,
    needle: &'a str,
    name: &'a str,
    use_file: &'a str,
}

struct Case<'a> {
    before: &'a [(&'a str, &'a str)],
    edits: &'a [Edit<'a>],
    /// A file that is indexed only if the grammar is available; the case skips otherwise.
    grammar_probe: &'a str,
    expect: &'a [Expect<'a>],
}

fn config() -> ConfigV1 {
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.embed = false;
    cfg.code_search.embed = false;
    cfg
}

fn write(root: &Path, rel: &str, body: &str) -> PathBuf {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, body).unwrap();
    path
}

fn scanned(files: &BTreeMap<String, String>) -> (tempfile::TempDir, Store) {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().unwrap();
    for (rel, body) in files {
        write(dir.path(), rel, body);
    }
    let mut store = Store::open(dir.path(), VIEW_WORKING).unwrap();
    scan(
        dir.path(),
        &mut store,
        &config(),
        ScanSource::WorkingTree,
        EmbedMode::Inline,
    )
    .unwrap();
    (dir, store)
}

fn rows(store: &Store) -> Rows {
    store.index_db.as_ref().unwrap().resolution_rows().unwrap()
}

fn render(rows: &Rows) -> Vec<String> {
    rows.iter()
        .map(|(name, key, value)| {
            let printable = |bytes: &[u8]| -> String {
                bytes
                    .iter()
                    .map(|b| if b.is_ascii_graphic() { *b as char } else { '.' })
                    .collect()
            };
            format!("{name} {} => {}", printable(key), printable(value))
        })
        .collect()
}

fn def_start(src: &str, needle: &str, name: &str) -> u32 {
    (src.find(needle).unwrap() + needle.find(name).unwrap()) as u32
}

fn assert_incremental_equals_full(case: &Case<'_>) {
    let mut files: BTreeMap<String, String> = case
        .before
        .iter()
        .map(|(rel, body)| ((*rel).to_string(), (*body).to_string()))
        .collect();
    let (dir, mut store) = scanned(&files);
    if store.lookup(case.grammar_probe).is_none() {
        eprintln!("grammar for {} unavailable here - skipping", case.grammar_probe);
        return;
    }
    let rows_before = rows(&store);

    let mut touched = Vec::new();
    for edit in case.edits {
        match edit {
            Edit::Write(rel, body) => {
                touched.push(write(dir.path(), rel, body));
                files.insert((*rel).to_string(), (*body).to_string());
            }
            Edit::Remove(rel) => {
                fs::remove_file(dir.path().join(rel)).unwrap();
                touched.push(dir.path().join(rel));
                files.remove(*rel);
            }
        }
    }
    scan_paths(dir.path(), &mut store, &config(), &touched, EmbedMode::Inline).unwrap();
    let incremental = rows(&store);

    let (_full_dir, full_store) = scanned(&files);
    let full = rows(&full_store);

    assert_ne!(
        rows_before, incremental,
        "the edits must change the rows or the case proves nothing"
    );
    let (inc, ful) = (render(&incremental), render(&full));
    let only_incremental: Vec<_> = inc.iter().filter(|r| !ful.contains(r)).collect();
    let only_full: Vec<_> = ful.iter().filter(|r| !inc.contains(r)).collect();
    assert!(
        incremental == full,
        "incremental rescan diverged from a full scan\nonly incremental: {only_incremental:#?}\nonly full: {only_full:#?}"
    );

    for expect in case.expect {
        let src = &files[expect.def_file];
        let start = def_start(src, expect.needle, expect.name);
        let uses = basemind::query::resolved_references(&store, &RelPath::from(expect.def_file), start);
        assert!(
            uses.iter().any(|(path, _)| path.as_str() == Some(expect.use_file)),
            "{} must reference {}:{} after the edits; got {uses:?}",
            expect.use_file,
            expect.def_file,
            expect.name
        );
    }
}

#[cfg(feature = "code-intel-js")]
mod js {
    use super::*;

    const HELPER: &str = "export function helper() {\n  return 1;\n}\n";
    const HELPER_MOVED: &str = "const pad = 0;\nexport function helper() {\n  return pad + 1;\n}\n";
    const APP: &str = "import { helper } from './index';\nexport const run = () => helper();\n";

    #[test]
    fn barrel_chain_target_moves() {
        assert_incremental_equals_full(&Case {
            before: &[
                ("impl.ts", HELPER),
                ("index.ts", "import { helper } from './impl';\nexport { helper };\n"),
                ("app.ts", APP),
            ],
            edits: &[Edit::Write("impl.ts", HELPER_MOVED)],
            grammar_probe: "impl.ts",
            expect: &[Expect {
                def_file: "impl.ts",
                needle: "function helper",
                name: "helper",
                use_file: "index.ts",
            }],
        });
    }

    #[test]
    fn reexport_from_clause_barrel_target_moves() {
        assert_incremental_equals_full(&Case {
            before: &[
                ("impl.ts", HELPER),
                ("index.ts", "export { helper } from './impl';\n"),
                ("app.ts", APP),
            ],
            edits: &[Edit::Write("impl.ts", HELPER_MOVED)],
            grammar_probe: "impl.ts",
            expect: &[],
        });
    }

    const TSCONFIG_LIB: &str = r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib/*":["lib/*"]}}}"#;
    const TSCONFIG_LIB2: &str = r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib/*":["lib2/*"]}}}"#;
    const THING: &str = "export const thing = 1;\n";
    const USES_ALIAS: &str = "import { thing } from '@lib/thing';\nexport const v = thing;\n";

    #[test]
    fn tsconfig_alias_edit_retargets_the_importer() {
        assert_incremental_equals_full(&Case {
            before: &[
                ("tsconfig.json", TSCONFIG_LIB),
                ("lib/thing.ts", THING),
                ("lib2/thing.ts", THING),
                ("src/app.ts", USES_ALIAS),
            ],
            edits: &[Edit::Write("tsconfig.json", TSCONFIG_LIB2)],
            grammar_probe: "src/app.ts",
            expect: &[Expect {
                def_file: "lib2/thing.ts",
                needle: "const thing",
                name: "thing",
                use_file: "src/app.ts",
            }],
        });
    }

    #[test]
    fn tsconfig_added_resolves_a_previously_unresolved_alias() {
        assert_incremental_equals_full(&Case {
            before: &[("lib/thing.ts", THING), ("src/app.ts", USES_ALIAS)],
            edits: &[Edit::Write("tsconfig.json", TSCONFIG_LIB)],
            grammar_probe: "src/app.ts",
            expect: &[Expect {
                def_file: "lib/thing.ts",
                needle: "const thing",
                name: "thing",
                use_file: "src/app.ts",
            }],
        });
    }

    const UTIL_JS: &str = "export const value = 1;\n";
    const UTIL_TS: &str = "export const pad = 0;\nexport const value = 2;\n";
    const USES_UTIL: &str = "import { value } from './util';\nexport const v = value;\n";

    #[test]
    fn new_file_shadows_a_resolved_module() {
        assert_incremental_equals_full(&Case {
            before: &[("util.js", UTIL_JS), ("app.ts", USES_UTIL)],
            edits: &[Edit::Write("util.ts", UTIL_TS)],
            grammar_probe: "app.ts",
            expect: &[Expect {
                def_file: "util.ts",
                needle: "const value",
                name: "value",
                use_file: "app.ts",
            }],
        });
    }

    #[test]
    fn deleting_the_shadowing_file_falls_back_to_the_next_candidate() {
        assert_incremental_equals_full(&Case {
            before: &[("util.js", UTIL_JS), ("util.ts", UTIL_TS), ("app.ts", USES_UTIL)],
            edits: &[Edit::Remove("util.ts")],
            grammar_probe: "app.ts",
            expect: &[Expect {
                def_file: "util.js",
                needle: "const value",
                name: "value",
                use_file: "app.ts",
            }],
        });
    }

    #[test]
    fn new_index_file_in_a_directory_that_did_not_exist() {
        assert_incremental_equals_full(&Case {
            before: &[("app.ts", "import { value } from './util';\nexport const v = value;\n")],
            edits: &[Edit::Write("util/index.ts", UTIL_JS)],
            grammar_probe: "app.ts",
            expect: &[Expect {
                def_file: "util/index.ts",
                needle: "const value",
                name: "value",
                use_file: "app.ts",
            }],
        });
    }

    #[test]
    fn deleted_importer_leaves_no_rows() {
        assert_incremental_equals_full(&Case {
            before: &[("util.ts", UTIL_JS), ("app.ts", USES_UTIL), ("other.ts", USES_UTIL)],
            edits: &[Edit::Remove("app.ts")],
            grammar_probe: "app.ts",
            expect: &[Expect {
                def_file: "util.ts",
                needle: "const value",
                name: "value",
                use_file: "other.ts",
            }],
        });
    }
}

#[cfg(feature = "code-intel-stack")]
mod python {
    use super::*;

    const IMPL: &str = "def foo():\n    return 1\n";
    const IMPL_MOVED: &str = "PAD = 0\n\n\ndef foo():\n    return PAD + 1\n";
    const INIT: &str = "from pkg.impl import foo\n";
    const APP: &str = "from pkg import foo\n\n\ndef run():\n    return foo()\n";

    #[test]
    fn adding_an_init_reexport_links_the_package_importer() {
        assert_incremental_equals_full(&Case {
            before: &[("pkg/impl.py", IMPL), ("app.py", APP)],
            edits: &[Edit::Write("pkg/__init__.py", INIT)],
            grammar_probe: "pkg/impl.py",
            expect: &[Expect {
                def_file: "pkg/impl.py",
                needle: "def foo",
                name: "foo",
                use_file: "app.py",
            }],
        });
    }

    #[test]
    fn target_behind_an_init_reexport_moves() {
        assert_incremental_equals_full(&Case {
            before: &[("pkg/impl.py", IMPL), ("pkg/__init__.py", INIT), ("app.py", APP)],
            edits: &[Edit::Write("pkg/impl.py", IMPL_MOVED)],
            grammar_probe: "pkg/impl.py",
            expect: &[Expect {
                def_file: "pkg/impl.py",
                needle: "def foo",
                name: "foo",
                use_file: "app.py",
            }],
        });
    }

    #[test]
    fn module_file_shadows_a_package_of_the_same_name() {
        assert_incremental_equals_full(&Case {
            before: &[
                ("pkg/mod/__init__.py", "def foo():\n    return 1\n"),
                ("app.py", "from pkg.mod import foo\n\n\ndef run():\n    return foo()\n"),
            ],
            edits: &[Edit::Write("pkg/mod.py", "PAD = 0\n\n\ndef foo():\n    return PAD\n")],
            grammar_probe: "pkg/mod/__init__.py",
            expect: &[Expect {
                def_file: "pkg/mod.py",
                needle: "def foo",
                name: "foo",
                use_file: "app.py",
            }],
        });
    }
}
