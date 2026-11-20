//! [`SpecifierResolver::resolve_submodule`]: a `from` import's name read as a submodule.

use std::fs;

use tempfile::TempDir;

use super::*;

fn from_import(specifier: &str, imported: Option<&str>) -> ImportEdge {
    ImportEdge {
        local: imported.unwrap_or("mod").to_string(),
        specifier: specifier.to_string(),
        imported: imported.map(str::to_string),
        is_type: false,
        local_start: 0,
        module_only: false,
    }
}

fn write(dir: &Path, rel: &str) {
    let abs = dir.join(rel);
    fs::create_dir_all(abs.parent().unwrap()).unwrap();
    fs::write(abs, "x = 1\n").unwrap();
}

fn submodule(
    language: &str,
    files: &[&str],
    importer: &str,
    package: Option<&str>,
    import: &ImportEdge,
) -> (Option<String>, ProbeSet) {
    let tmp = TempDir::new().unwrap();
    for file in files {
        write(tmp.path(), file);
    }
    let resolver = SpecifierResolver::for_language(language).unwrap();
    let package = package.map(RelPath::from);
    let mut probes = ProbeSet::default();
    let got = resolver.resolve_submodule(tmp.path(), importer, package.as_ref(), import, &mut probes);
    (got.and_then(|rel| rel.as_str().map(str::to_string)), probes)
}

#[test]
fn python_from_import_name_resolves_as_a_submodule() {
    let cases: &[(&[&str], &str, Option<&str>, &str, &str, Option<&str>)] = &[
        (
            &["pkg/__init__.py", "pkg/views.py", "pkg/app.py"],
            "pkg/app.py",
            Some("pkg/__init__.py"),
            ".",
            "views",
            Some("pkg/views.py"),
        ),
        (
            &["pkg/__init__.py", "pkg/sub/__init__.py"],
            "app.py",
            Some("pkg/__init__.py"),
            "pkg",
            "sub",
            Some("pkg/sub/__init__.py"),
        ),
        (
            &["pkg/sub/__init__.py", "pkg/sub/leaf.py"],
            "pkg/app.py",
            Some("pkg/sub/__init__.py"),
            ".sub",
            "leaf",
            Some("pkg/sub/leaf.py"),
        ),
        (
            &["pkg/inner/__init__.py", "pkg/other.py"],
            "pkg/inner/app.py",
            Some("pkg/inner/__init__.py"),
            "..",
            "other",
            Some("pkg/other.py"),
        ),
        (
            &["pkg/__init__.py", "pkg/sub.py", "src/pkg/sub.py"],
            "app.py",
            Some("pkg/__init__.py"),
            "pkg",
            "sub",
            Some("pkg/sub.py"),
        ),
        (&["ns/views.py"], "app.py", None, "ns", "views", Some("ns/views.py")),
        (
            &["src/ns/views.py"],
            "app.py",
            None,
            "ns",
            "views",
            Some("src/ns/views.py"),
        ),
        (&["pkg.py", "pkg/sub.py"], "app.py", Some("pkg.py"), "pkg", "sub", None),
        (
            &["pkg/__init__.py"],
            "app.py",
            Some("pkg/__init__.py"),
            "pkg",
            "gone",
            None,
        ),
    ];
    for &(files, importer, package, specifier, name, expected) in cases {
        let import = from_import(specifier, Some(name));
        let (got, _) = submodule("python", files, importer, package, &import);
        assert_eq!(
            got.as_deref(),
            expected,
            "from {specifier} import {name} in {importer}, package {package:?}"
        );
    }
}

#[test]
fn python_module_import_names_no_submodule() {
    let (got, _) = submodule(
        "python",
        &["pkg/__init__.py", "pkg/mod.py"],
        "app.py",
        Some("pkg/__init__.py"),
        &from_import("pkg", None),
    );
    assert_eq!(got, None);
}

#[test]
fn python_submodule_miss_records_both_candidates() {
    let (got, probes) = submodule(
        "python",
        &["pkg/__init__.py"],
        "app.py",
        Some("pkg/__init__.py"),
        &from_import("pkg", Some("views")),
    );
    assert_eq!(got, None);
    for candidate in ["pkg/views.py", "pkg/views/__init__.py"] {
        assert!(probes.contains(candidate), "{candidate} must be a recorded probe");
    }
    assert!(
        !probes.contains("src/pkg/views.py"),
        "a package's submodule is not looked for under another root"
    );
}

#[test]
fn java_has_no_submodule_reading() {
    let (got, _) = submodule(
        "java",
        &["com/example/Foo/Bar.java"],
        "Main.java",
        None,
        &from_import("com.example.Foo", Some("Bar")),
    );
    assert_eq!(got, None);
}
