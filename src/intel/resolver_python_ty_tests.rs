//! [`python_ty::TyResolver`] over an indexed file list, and the stitch's Python resolver that asks
//! it first and falls back to path arithmetic.

use std::fs;

use tempfile::TempDir;

use super::python_ty::TyResolver;
use super::*;

/// The resolver over `files` plus `importer`, which must be indexed to anchor its imports.
fn ty_over(files: &[&str], importer: &str) -> TyResolver {
    TyResolver::new(files.iter().copied().chain([importer])).expect("the ty resolver builds")
}

fn owned(got: Option<RelPath>) -> Option<String> {
    got.and_then(|rel| rel.as_str().map(str::to_string))
}

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

/// `files` written to a temporary repository, for the path-arithmetic fallback that reads the disk,
/// and the stitch's ty-backed Python resolver over the same files.
fn stitch_resolver(files: &[&str]) -> (TempDir, SpecifierResolver) {
    let tmp = TempDir::new().unwrap();
    for rel in files {
        let abs = tmp.path().join(rel);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(abs, "x = 1\n").unwrap();
    }
    let resolver = SpecifierResolver::Python(python::PythonResolver::with_ty(files.iter().copied()));
    (tmp, resolver)
}

/// `(indexed files, importer, specifier, expected target)` for one import.
type ImportCase<'a> = (&'a [&'a str], &'a str, &'a str, Option<&'a str>);

#[test]
fn ty_resolves_each_import_form_to_its_indexed_file() {
    let cases: &[ImportCase<'_>] = &[
        (&["pkg/__init__.py", "pkg/b.py"], "pkg/a.py", ".b", Some("pkg/b.py")),
        (&["pkg/__init__.py"], "pkg/a.py", ".", Some("pkg/__init__.py")),
        (
            &["pkg/__init__.py", "pkg/inner/__init__.py", "pkg/other.py"],
            "pkg/inner/app.py",
            "..other",
            Some("pkg/other.py"),
        ),
        (
            &["pkg/__init__.py", "pkg/mod.py"],
            "app.py",
            "pkg",
            Some("pkg/__init__.py"),
        ),
        (
            &["pkg/__init__.py", "pkg/mod.py"],
            "app.py",
            "pkg.mod",
            Some("pkg/mod.py"),
        ),
        (&["pkg/__init__.py", "pkg.py"], "app.py", "pkg", Some("pkg/__init__.py")),
        (
            &["src/pkg/__init__.py", "src/pkg/mod.py"],
            "tests/test_mod.py",
            "pkg.mod",
            Some("src/pkg/mod.py"),
        ),
        (&["ns/views.py"], "app.py", "ns.views", Some("ns/views.py")),
        (&["ns/views.py"], "app.py", "ns", None),
        (&["typed/__init__.pyi"], "app.py", "typed", Some("typed/__init__.pyi")),
        (&["m.py", "m.pyi"], "app.py", "m", Some("m.py")),
        (
            &["services/billing/app/__init__.py", "services/billing/app/models.py"],
            "services/billing/app/main.py",
            "app.models",
            Some("services/billing/app/models.py"),
        ),
        (&[], "app.py", "os", None),
        (&[], "app.py", "missing.mod", None),
        (&[], "app.py", "..beyond", None),
    ];
    for &(files, importer, specifier, expected) in cases {
        let resolver = ty_over(files, importer);
        let got = owned(resolver.resolve(importer, specifier, &mut ProbeSet::default()));
        assert_eq!(
            got.as_deref(),
            expected,
            "{specifier} imported by {importer} over {files:?}"
        );
    }
}

/// `(indexed files, package file, submodule name, expected target)` for one submodule.
type SubmoduleCase<'a> = (&'a [&'a str], &'a str, &'a str, Option<&'a str>);

#[test]
fn ty_resolves_a_submodule_in_its_own_package() {
    let cases: &[SubmoduleCase<'_>] = &[
        (&["pkg/views.py"], "pkg/__init__.py", "views", Some("pkg/views.py")),
        (
            &["pkg/sub/__init__.py"],
            "pkg/__init__.py",
            "sub",
            Some("pkg/sub/__init__.py"),
        ),
        (
            &["src/pkg/sub.py", "pkg/sub.py"],
            "src/pkg/__init__.py",
            "sub",
            Some("src/pkg/sub.py"),
        ),
        (&["pkg/views.pyi"], "pkg/__init__.pyi", "views", Some("pkg/views.pyi")),
        (&["pkg/sub.py"], "pkg.py", "sub", None),
        (&[], "pkg/__init__.py", "gone", None),
    ];
    for &(files, package, name, expected) in cases {
        let resolver = ty_over(files, package);
        let got = owned(resolver.submodule(package, name, &mut ProbeSet::default()));
        assert_eq!(got.as_deref(), expected, "submodule {name} of {package} over {files:?}");
    }
}

#[test]
fn ty_hit_records_every_candidate_that_could_change_it() {
    let resolver = ty_over(&["src/pkg/__init__.py", "src/pkg/mod.py"], "app.py");
    let mut probes = ProbeSet::default();
    let got = owned(resolver.resolve("app.py", "pkg.mod", &mut probes));
    assert_eq!(got.as_deref(), Some("src/pkg/mod.py"));
    for probe in [
        "src/pkg-stubs",
        "src/pkg.py",
        "src/pkg.pyi",
        "src/pkg/__init__.py",
        "src/pkg/__init__.pyi",
        "src/pkg/mod.py",
        "src/pkg/mod.pyi",
        "src/pkg/mod",
        "pkg-stubs",
        "pkg.py",
        "pkg.pyi",
        "pkg",
    ] {
        assert!(probes.contains(probe), "{probe} must be a recorded probe");
    }
    assert!(
        !probes.contains("__init__.py"),
        "a hit under the configured roots searches no ancestor directory"
    );
}

#[test]
fn ty_ancestor_search_records_the_ancestor_packages() {
    let importer = "services/billing/app/main.py";
    let resolver = ty_over(
        &["services/billing/app/__init__.py", "services/billing/app/models.py"],
        importer,
    );
    let mut probes = ProbeSet::default();
    let got = owned(resolver.resolve(importer, "app.models", &mut probes));
    assert_eq!(got.as_deref(), Some("services/billing/app/models.py"));
    for probe in [
        "services/billing/app/__init__.py",
        "services/billing/__init__.py",
        "services/__init__.py",
        "__init__.py",
        "app",
        "services/billing/app/models.py",
        "services/app",
    ] {
        assert!(probes.contains(probe), "{probe} must be a recorded probe");
    }
}

#[test]
fn stitch_resolver_resolves_a_nested_project_import_the_path_arithmetic_misses() {
    let files = [
        "services/billing/app/__init__.py",
        "services/billing/app/models.py",
        "services/billing/app/main.py",
    ];
    let (tmp, resolver) = stitch_resolver(&files);
    let import = from_import("app.models", Some("Invoice"));
    let importer = "services/billing/app/main.py";

    let path_only = SpecifierResolver::for_language("python").unwrap();
    let missed = path_only.resolve(tmp.path(), importer, &import, &mut ProbeSet::default());
    assert_eq!(
        owned(missed),
        None,
        "the path arithmetic searches only `src/` and the root"
    );

    let got = resolver.resolve(tmp.path(), importer, &import, &mut ProbeSet::default());
    assert_eq!(owned(got).as_deref(), Some("services/billing/app/models.py"));
}

/// `(indexed files, importer, specifier, package the module resolved to, expected submodule)`.
type FromDotCase<'a> = (&'a [&'a str], &'a str, &'a str, Option<&'a str>, &'a str);

#[test]
fn stitch_resolver_resolves_a_from_import_name_to_the_submodule() {
    let cases: &[FromDotCase<'_>] = &[
        (
            &["pkg/__init__.py", "pkg/views.py", "pkg/app.py"],
            "pkg/app.py",
            ".",
            Some("pkg/__init__.py"),
            "pkg/views.py",
        ),
        (&["ns/views.py", "ns/app.py"], "ns/app.py", ".", None, "ns/views.py"),
        (
            &["pkg/inner/__init__.py", "pkg/inner/views.py", "pkg/inner/app.py"],
            "pkg/inner/app.py",
            "..inner",
            Some("pkg/inner/__init__.py"),
            "pkg/inner/views.py",
        ),
    ];
    for &(files, importer, specifier, package, expected) in cases {
        let (tmp, resolver) = stitch_resolver(files);
        let import = from_import(specifier, Some("views"));
        let mut probes = ProbeSet::default();
        let module = resolver.resolve(tmp.path(), importer, &import, &mut probes);
        assert_eq!(
            owned(module).as_deref(),
            package,
            "from {specifier} import views in {importer}"
        );
        let package = package.map(RelPath::from);
        let got = resolver.resolve_submodule(tmp.path(), importer, package.as_ref(), &import, &mut probes);
        assert_eq!(
            owned(got).as_deref(),
            Some(expected),
            "from {specifier} import views in {importer}, package {package:?}"
        );
    }
}

#[test]
fn stitch_resolver_falls_back_to_path_arithmetic_when_ty_misses() {
    let files = ["my-service/app/x.py", "my-service/app/y.py"];
    let importer = "my-service/app/x.py";
    let ty_alone = ty_over(&files, importer);
    assert_eq!(
        owned(ty_alone.resolve(importer, ".y", &mut ProbeSet::default())),
        None,
        "`my-service` is no module name, so ty cannot anchor the relative import"
    );

    let (tmp, resolver) = stitch_resolver(&files);
    let mut probes = ProbeSet::default();
    let got = resolver.resolve(tmp.path(), importer, &from_import(".y", Some("z")), &mut probes);
    assert_eq!(owned(got).as_deref(), Some("my-service/app/y.py"));
    assert!(
        probes.contains("my-service/app/y.py"),
        "the fallback records its candidates"
    );
}
