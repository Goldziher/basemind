//! Tests for JS/TS workspace package resolution: a two-package fixture with no `node_modules`.

use std::fs;
use std::path::Path;

use serde_json::json;
use tempfile::TempDir;

use super::js_workspace::{Workspace, entry_candidates};
use super::*;

fn import(specifier: &str) -> ImportEdge {
    ImportEdge {
        local: "x".to_string(),
        specifier: specifier.to_string(),
        imported: Some("x".to_string()),
        is_type: false,
        local_start: 0,
        module_only: false,
    }
}

fn write(dir: &Path, rel: &str, body: &str) {
    let abs = dir.join(rel);
    fs::create_dir_all(abs.parent().unwrap()).unwrap();
    fs::write(abs, body).unwrap();
}

/// Resolve `specifier` from `importer` and return the target path and the probes.
fn resolve(root: &Path, importer: &str, specifier: &str) -> (Option<String>, ProbeSet) {
    let resolver = SpecifierResolver::for_language("typescript").unwrap();
    resolver.begin_importer(importer);
    let mut probes = ProbeSet::default();
    let hit = resolver.resolve(root, importer, &import(specifier), &mut probes);
    (hit.and_then(|rel| rel.as_str().map(str::to_string)), probes)
}

/// Two packages, `@acme/app` importing `@acme/lib`, declared by `declaration` (a root file).
fn two_packages(declaration: (&str, &str)) -> TempDir {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), declaration.0, declaration.1);
    write(tmp.path(), "packages/app/package.json", r#"{"name":"@acme/app"}"#);
    write(tmp.path(), "packages/app/src/main.ts", "");
    write(
        tmp.path(),
        "packages/lib/package.json",
        r#"{"name":"@acme/lib","main":"dist/index.js"}"#,
    );
    write(tmp.path(), "packages/lib/src/index.ts", "export const lib = 1;\n");
    write(tmp.path(), "packages/lib/src/format.ts", "export const format = 1;\n");
    tmp
}

const PNPM: (&str, &str) = ("pnpm-workspace.yaml", "packages:\n  - 'packages/*'\n");
const NPM: (&str, &str) = ("package.json", r#"{"private":true,"workspaces":["packages/*"]}"#);
const YARN: (&str, &str) = ("package.json", r#"{"workspaces":{"packages":["packages/*"]}}"#);

#[test]
fn a_package_imports_a_sibling_package_by_name_without_node_modules() {
    for declaration in [PNPM, NPM, YARN] {
        let tmp = two_packages(declaration);
        let (hit, probes) = resolve(tmp.path(), "packages/app/src/main.ts", "@acme/lib");
        assert_eq!(hit.as_deref(), Some("packages/lib/src/index.ts"), "{declaration:?}");
        assert!(probes.contains("packages/lib/package.json"));
        assert!(probes.contains(declaration.0));
    }
}

#[test]
fn a_subpath_of_a_package_resolves_under_its_source_directory() {
    let tmp = two_packages(PNPM);
    let (hit, _) = resolve(tmp.path(), "packages/app/src/main.ts", "@acme/lib/format");
    assert_eq!(hit.as_deref(), Some("packages/lib/src/format.ts"));
}

#[test]
fn the_exports_map_of_a_package_decides_its_entry() {
    let tmp = two_packages(PNPM);
    write(
        tmp.path(),
        "packages/lib/package.json",
        r#"{"name":"@acme/lib","exports":{".":{"types":"./src/format.ts"},"./util":"./src/util.ts"}}"#,
    );
    write(tmp.path(), "packages/lib/src/util.ts", "");
    let (entry, _) = resolve(tmp.path(), "packages/app/src/main.ts", "@acme/lib");
    let (util, _) = resolve(tmp.path(), "packages/app/src/main.ts", "@acme/lib/util");
    assert_eq!(entry.as_deref(), Some("packages/lib/src/format.ts"));
    assert_eq!(util.as_deref(), Some("packages/lib/src/util.ts"));
}

#[test]
fn a_package_outside_the_declared_globs_is_not_a_workspace_package() {
    let tmp = two_packages(PNPM);
    write(tmp.path(), "tools/cli/package.json", r#"{"name":"@acme/cli"}"#);
    write(tmp.path(), "tools/cli/index.ts", "");
    let (hit, _) = resolve(tmp.path(), "packages/app/src/main.ts", "@acme/cli");
    assert_eq!(hit, None);
}

#[test]
fn an_excluded_package_is_not_a_workspace_package() {
    let tmp = two_packages(("pnpm-workspace.yaml", "packages:\n  - 'packages/*'\n  - '!packages/lib'\n"));
    let (hit, _) = resolve(tmp.path(), "packages/app/src/main.ts", "@acme/lib");
    assert_eq!(hit, None);
}

#[test]
fn a_repository_with_no_declaration_has_no_workspace_packages() {
    let tmp = two_packages(("README.md", ""));
    let (hit, _) = resolve(tmp.path(), "packages/app/src/main.ts", "@acme/lib");
    assert_eq!(hit, None);
}

#[test]
fn a_relative_specifier_never_consults_the_workspace() {
    let tmp = two_packages(PNPM);
    write(tmp.path(), "packages/app/src/util.ts", "");
    let (hit, _) = resolve(tmp.path(), "packages/app/src/main.ts", "./util");
    assert_eq!(hit.as_deref(), Some("packages/app/src/util.ts"));
}

/// An installed workspace link is a symlink into the repository: it resolves to the real path.
#[cfg(unix)]
#[test]
fn a_symlink_into_the_repository_resolves_to_its_real_path() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "real/thing.ts", "export const x = 1;\n");
    write(tmp.path(), "app/main.ts", "");
    fs::create_dir_all(tmp.path().join("app/vendor")).unwrap();
    std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("app/vendor/linked")).unwrap();
    let (hit, _) = resolve(tmp.path(), "app/main.ts", "./vendor/linked/thing");
    assert_eq!(hit.as_deref(), Some("real/thing.ts"));
}

#[test]
fn locate_splits_a_specifier_into_package_and_subpath() {
    let tmp = two_packages(PNPM);
    write(tmp.path(), "packages/lib-b/package.json", r#"{"name":"lib-b"}"#);
    let workspace = Workspace::discover(tmp.path());
    let cases = [
        ("@acme/lib", Some(("packages/lib", ""))),
        ("@acme/lib/a/b", Some(("packages/lib", "a/b"))),
        ("lib-b/x", Some(("packages/lib-b", "x"))),
        ("@acme", None),
        ("react", None),
    ];
    for (specifier, expected) in cases {
        assert_eq!(workspace.locate(specifier), expected, "{specifier}");
    }
}

#[test]
fn entry_candidates_prefer_exports_then_manifest_fields_then_conventions() {
    let manifest = json!({"main": "dist/index.js", "module": "./esm/index.js", "exports": {"import": "./x.js"}});
    assert_eq!(
        entry_candidates(&manifest, ""),
        ["./x.js", "./esm/index.js", "./dist/index.js", "./src/index", "./index"]
    );
    assert_eq!(entry_candidates(&manifest, "a/b"), ["./a/b", "./src/a/b"]);
}
