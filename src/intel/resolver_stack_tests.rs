use std::fs;

use tempfile::TempDir;

use super::*;
use crate::intel::model::ImportEdge;

fn import(specifier: &str, imported: Option<&str>) -> ImportEdge {
    ImportEdge {
        local: imported.unwrap_or("mod").to_string(),
        specifier: specifier.to_string(),
        imported: imported.map(str::to_string),
        is_type: false,
        local_start: 0,
        module_only: false,
    }
}

/// Resolve without caring which paths were probed.
fn resolve_unprobed(
    resolver: &SpecifierResolver,
    root: &Path,
    importer_rel: &str,
    import: &ImportEdge,
) -> Option<RelPath> {
    resolver.resolve(root, importer_rel, import, &mut ProbeSet::default())
}

fn write(dir: &Path, rel: &str, body: &str) {
    let abs = dir.join(rel);
    fs::create_dir_all(abs.parent().unwrap()).unwrap();
    fs::write(abs, body).unwrap();
}

#[test]
fn python_absolute_module_resolves_to_py_file() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "foo/bar.py", "x = 1\n");
    write(root, "foo/__init__.py", "");
    let resolver = SpecifierResolver::for_language("python").expect("python resolver");
    let got = resolve_unprobed(&resolver, root, "app/main.py", &import("foo.bar", Some("x")));
    assert_eq!(
        got.and_then(|r| r.as_str().map(str::to_string)),
        Some("foo/bar.py".to_string())
    );
}

#[test]
fn python_absolute_module_resolves_to_package_init() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "pkg/__init__.py", "y = 2\n");
    let resolver = SpecifierResolver::for_language("python").unwrap();
    let got = resolve_unprobed(&resolver, root, "app/main.py", &import("pkg", Some("y")));
    assert_eq!(
        got.and_then(|r| r.as_str().map(str::to_string)),
        Some("pkg/__init__.py".to_string())
    );
}

#[test]
fn python_absolute_module_prefers_src_layout() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "src/mypkg/util.py", "z = 3\n");
    let resolver = SpecifierResolver::for_language("python").unwrap();
    let got = resolve_unprobed(&resolver, root, "src/mypkg/main.py", &import("mypkg.util", Some("z")));
    assert_eq!(
        got.and_then(|r| r.as_str().map(str::to_string)),
        Some("src/mypkg/util.py".to_string())
    );
}

#[test]
fn python_relative_single_dot_resolves_sibling() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "pkg/sibling.py", "a = 1\n");
    write(root, "pkg/main.py", "");
    let resolver = SpecifierResolver::for_language("python").unwrap();
    let got = resolve_unprobed(&resolver, root, "pkg/main.py", &import(".sibling", Some("a")));
    assert_eq!(
        got.and_then(|r| r.as_str().map(str::to_string)),
        Some("pkg/sibling.py".to_string())
    );
}

#[test]
fn python_relative_bare_dot_resolves_package_init() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "pkg/__init__.py", "b = 1\n");
    write(root, "pkg/main.py", "");
    let resolver = SpecifierResolver::for_language("python").unwrap();
    let got = resolve_unprobed(&resolver, root, "pkg/main.py", &import(".", Some("b")));
    assert_eq!(
        got.and_then(|r| r.as_str().map(str::to_string)),
        Some("pkg/__init__.py".to_string())
    );
}

#[test]
fn python_relative_double_dot_climbs_package() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "pkg/other.py", "c = 1\n");
    write(root, "pkg/sub/main.py", "");
    let resolver = SpecifierResolver::for_language("python").unwrap();
    let got = resolve_unprobed(&resolver, root, "pkg/sub/main.py", &import("..other", Some("c")));
    assert_eq!(
        got.and_then(|r| r.as_str().map(str::to_string)),
        Some("pkg/other.py".to_string())
    );
}

#[test]
fn python_relative_cannot_climb_above_root() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "main.py", "");
    let resolver = SpecifierResolver::for_language("python").unwrap();
    let got = resolve_unprobed(&resolver, root, "main.py", &import("..x", Some("x")));
    assert_eq!(got, None);
}

#[test]
fn python_missing_module_resolves_none() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let resolver = SpecifierResolver::for_language("python").unwrap();
    let got = resolve_unprobed(&resolver, root, "main.py", &import("nope.gone", Some("x")));
    assert_eq!(got, None);
}

#[test]
fn java_fqn_resolves_under_maven_root() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        root,
        "src/main/java/com/example/Foo.java",
        "package com.example;\nclass Foo {}\n",
    );
    let resolver = SpecifierResolver::for_language("java").expect("java resolver");
    let got = resolve_unprobed(
        &resolver,
        root,
        "src/main/java/com/example/Bar.java",
        &import("com.example.Foo", None),
    );
    assert_eq!(
        got.and_then(|r| r.as_str().map(str::to_string)),
        Some("src/main/java/com/example/Foo.java".to_string())
    );
}

#[test]
fn java_fqn_resolves_at_repo_root() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "com/example/Baz.java", "package com.example;\nclass Baz {}\n");
    let resolver = SpecifierResolver::for_language("java").unwrap();
    let got = resolve_unprobed(
        &resolver,
        root,
        "com/example/Main.java",
        &import("com.example.Baz", None),
    );
    assert_eq!(
        got.and_then(|r| r.as_str().map(str::to_string)),
        Some("com/example/Baz.java".to_string())
    );
}

#[test]
fn java_wildcard_import_resolves_none() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "src/main/java/com/example/Foo.java", "class Foo {}\n");
    let resolver = SpecifierResolver::for_language("java").unwrap();
    let got = resolve_unprobed(
        &resolver,
        root,
        "src/main/java/com/example/Bar.java",
        &import("com.example.*", None),
    );
    assert_eq!(got, None);
}

#[test]
fn java_missing_type_resolves_none() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let resolver = SpecifierResolver::for_language("java").unwrap();
    let got = resolve_unprobed(&resolver, root, "Main.java", &import("com.example.Gone", None));
    assert_eq!(got, None);
}

fn probes_of(resolver: &SpecifierResolver, root: &Path, importer: &str, import: &ImportEdge) -> ProbeSet {
    let mut probes = ProbeSet::default();
    resolver.resolve(root, importer, import, &mut probes);
    probes
}

#[test]
fn python_miss_records_every_candidate_it_tried() {
    let tmp = TempDir::new().unwrap();
    let resolver = SpecifierResolver::for_language("python").unwrap();
    let probes = probes_of(&resolver, tmp.path(), "main.py", &import("nope.gone", Some("x")));
    for candidate in [
        "src/nope/gone.py",
        "src/nope/gone/__init__.py",
        "nope/gone.py",
        "nope/gone/__init__.py",
    ] {
        assert!(probes.contains(candidate), "{candidate} must be a recorded probe");
    }
}

/// The module file beats the package, so the package hit alone is not the dependency: the
/// importer must also be found by the `pkg/mod.py` that would shadow it.
#[test]
fn python_package_hit_also_records_the_shadowing_module_candidate() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "pkg/mod/__init__.py", "x = 1\n");
    let resolver = SpecifierResolver::for_language("python").unwrap();
    let import = import("pkg.mod", Some("x"));
    let probes = probes_of(&resolver, tmp.path(), "main.py", &import);
    assert!(probes.contains("pkg/mod.py"));
    assert!(probes.contains("pkg/mod/__init__.py"));
}

#[test]
fn java_miss_records_a_candidate_per_source_root() {
    let tmp = TempDir::new().unwrap();
    let resolver = SpecifierResolver::for_language("java").unwrap();
    let probes = probes_of(&resolver, tmp.path(), "Main.java", &import("com.example.Gone", None));
    for candidate in [
        "src/main/java/com/example/Gone.java",
        "src/test/java/com/example/Gone.java",
        "src/com/example/Gone.java",
        "com/example/Gone.java",
    ] {
        assert!(probes.contains(candidate), "{candidate} must be a recorded probe");
    }
}

fn multi_root_fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/multi_root")
}

fn resolved_in(root: &Path, language: &str, importer: &str, specifier: &str) -> Option<String> {
    let resolver = SpecifierResolver::for_language(language).unwrap();
    resolve_unprobed(&resolver, root, importer, &import(specifier, None))
        .and_then(|rel| rel.as_str().map(str::to_string))
}

#[test]
fn python_import_resolves_across_project_roots_and_to_stubs() {
    let root = multi_root_fixture();
    for (specifier, expected) in [
        ("beta.core", Some("py/b/src/beta/core.py")),
        ("beta", Some("py/b/src/beta/__init__.py")),
        ("beta.typed", Some("py/b/src/beta/typed.pyi")),
        ("beta.gone", None),
        ("gamma.core", None),
    ] {
        assert_eq!(
            resolved_in(&root, "python", "py/a/src/alpha/app.py", specifier).as_deref(),
            expected,
            "{specifier}"
        );
    }
}

#[test]
fn python_prefers_the_project_root_nearest_the_importer() {
    let tmp = TempDir::new().unwrap();
    for project in ["a", "b"] {
        write(tmp.path(), &format!("{project}/pyproject.toml"), "");
        write(tmp.path(), &format!("{project}/shared/mod.py"), "");
    }
    assert_eq!(
        resolved_in(tmp.path(), "python", "b/app.py", "shared.mod").as_deref(),
        Some("b/shared/mod.py")
    );
    assert_eq!(
        resolved_in(tmp.path(), "python", "a/app.py", "shared.mod").as_deref(),
        Some("a/shared/mod.py")
    );
}

#[test]
fn python_module_file_beats_its_stub() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "pkg/mod.py", "");
    write(tmp.path(), "pkg/mod.pyi", "");
    assert_eq!(
        resolved_in(tmp.path(), "python", "app.py", "pkg.mod").as_deref(),
        Some("pkg/mod.py")
    );
}

#[test]
fn java_import_resolves_across_project_roots() {
    let root = multi_root_fixture();
    for (specifier, expected) in [
        ("com.beta.Lib", Some("java/b/src/main/java/com/beta/Lib.java")),
        ("com.alpha.App", Some("java/a/src/main/java/com/alpha/App.java")),
        ("com.beta.Gone", None),
        ("org.other.Lib", None),
    ] {
        assert_eq!(
            resolved_in(&root, "java", "java/a/src/main/java/com/alpha/App.java", specifier).as_deref(),
            expected,
            "{specifier}"
        );
    }
}

#[test]
fn java_prefers_the_project_root_nearest_the_importer() {
    let tmp = TempDir::new().unwrap();
    for project in ["a", "b"] {
        write(tmp.path(), &format!("{project}/build.gradle.kts"), "");
        write(tmp.path(), &format!("{project}/src/main/java/com/x/T.java"), "");
    }
    assert_eq!(
        resolved_in(tmp.path(), "java", "b/src/main/java/com/y/U.java", "com.x.T").as_deref(),
        Some("b/src/main/java/com/x/T.java")
    );
}

#[test]
fn unknown_language_has_no_resolver() {
    assert!(SpecifierResolver::for_language("cobol").is_none());
    assert!(!SpecifierResolver::supports("cobol"));
}

#[test]
fn python_java_imports_have_no_default_export_fallback() {
    assert_eq!(
        SpecifierResolver::for_language("python").unwrap().default_export_name(),
        None,
        "Python has no default-export convention"
    );
    assert_eq!(
        SpecifierResolver::for_language("java").unwrap().default_export_name(),
        None,
        "Java has no default-export convention"
    );
}
