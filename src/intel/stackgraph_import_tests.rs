//! The import edges the per-language import queries extract, one case per import form.

use super::*;
use crate::intel::model::ImportEdge;

fn imports_of(lang: LangId, src: &str) -> Option<Vec<ImportEdge>> {
    if crate::lang::language(lang).is_err() {
        eprintln!("skip: {lang} grammar unavailable in this environment");
        return None;
    }
    let mut out = FileResolvedRefs::new(lang);
    populate_imports_exports(lang, src.as_bytes(), &mut out);
    let mut imports = out.imports;
    imports.sort_by_key(|import| import.local_start);
    Some(imports)
}

fn edge(local: &str, specifier: &str, imported: Option<&str>, local_start: u32) -> ImportEdge {
    ImportEdge {
        local: local.to_string(),
        specifier: specifier.to_string(),
        imported: imported.map(str::to_string),
        is_type: false,
        local_start,
        module_only: false,
    }
}

#[test]
fn python_import_forms_extract_one_edge_per_bound_name() {
    let cases: &[(&str, Vec<ImportEdge>)] = &[
        ("import os\n", vec![edge("os", "os", None, 7)]),
        ("import os.path\n", vec![edge("os", "os.path", None, 7)]),
        ("import a.b.c as n\n", vec![edge("n", "a.b.c", None, 16)]),
        (
            "import a, b.c as d\n",
            vec![edge("a", "a", None, 7), edge("d", "b.c", None, 17)],
        ),
        ("from m import f\n", vec![edge("f", "m", Some("f"), 14)]),
        ("from m import x as y\n", vec![edge("y", "m", Some("x"), 19)]),
        (
            "from m import f, x as y\n",
            vec![edge("f", "m", Some("f"), 14), edge("y", "m", Some("x"), 22)],
        ),
        ("from . import views\n", vec![edge("views", ".", Some("views"), 14)]),
        (
            "from .impl import helper as helper\n",
            vec![edge("helper", ".impl", Some("helper"), 28)],
        ),
        ("from ..pkg.mod import g\n", vec![edge("g", "..pkg.mod", Some("g"), 22)]),
        ("from m import *\n", vec![]),
    ];
    for (src, expected) in cases {
        let Some(imports) = imports_of("python", src) else {
            return;
        };
        assert_eq!(&imports, expected, "imports of {src:?}");
    }
}

#[test]
fn java_import_binds_the_simple_type_name() {
    let Some(imports) = imports_of("java", "import a.b.Foo;\nclass C {}\n") else {
        return;
    };
    assert_eq!(imports, vec![edge("Foo", "a.b.Foo", Some("Foo"), 11)]);
}
