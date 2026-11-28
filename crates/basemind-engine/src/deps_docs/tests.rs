use std::fs;
use std::path::Path;

use super::*;

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, text).expect("write");
}

fn roots(repo: &Path, homes: DependencyHomes, granted: bool) -> DependencyRoots {
    DependencyRoots {
        repo: repo.canonicalize().expect("canonical repo"),
        homes,
        granted,
    }
}

fn cfg() -> DependenciesConfig {
    DependenciesConfig {
        enabled: true,
        ..DependenciesConfig::default()
    }
}

fn reasons(docs: &DepDocs) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = docs
        .unavailable
        .iter()
        .map(|dep| (dep.name.clone(), dep.reason.clone()))
        .collect();
    out.sort();
    out
}

/// A repository depending on `react` (installed, with docs) and `left-pad` (not installed).
fn npm_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("repo");
    write(
        &repo.path().join("package.json"),
        r#"{"dependencies":{"react":"^18","left-pad":"^1"}}"#,
    );
    let react = repo.path().join("node_modules/react");
    write(&react.join("package.json"), r#"{"name":"react","version":"18.3.1"}"#);
    write(&react.join("README.md"), "# React\n");
    write(&react.join("docs/hooks.md"), "# Hooks\n");
    write(&react.join("index.js"), "module.exports = {};\n");
    repo
}

#[test]
fn virtual_paths_split_into_ecosystem_package_version_and_file() {
    let parsed = parse_virtual("deps/npm/react@18.3.1/docs/hooks.md").expect("parses");
    assert_eq!(
        parsed,
        VirtualPath {
            ecosystem: Ecosystem::Npm,
            name: "react",
            version: "18.3.1",
            file: "docs/hooks.md"
        }
    );
    let scoped = parse_virtual("deps/npm/@types/node@20.1.0/README.md").expect("scoped");
    assert_eq!(
        (scoped.name, scoped.version, scoped.file),
        ("@types/node", "20.1.0", "README.md")
    );
    let go = parse_virtual("deps/go/github.com/pkg/errors@v0.9.1/README.md").expect("go");
    assert_eq!((go.name, go.version), ("github.com/pkg/errors", "v0.9.1"));
    for not in [
        "deps/npm/react/README.md",
        "deps/elixir/plug@1.0/README.md",
        "deps/npm/react@18.3.1",
        "deps/npm/react@/README.md",
        "src/deps/npm/react@1/README.md",
        "deps/npm/@scope/README.md",
    ] {
        assert!(!is_virtual(not), "{not}");
    }
    let built = virtual_path(Ecosystem::Cargo, "serde", "1.0.200", "README.md");
    assert_eq!(built, "deps/cargo/serde@1.0.200/README.md");
    assert!(is_virtual(&built));
}

#[test]
fn an_installed_npm_package_yields_its_readme_and_docs_and_a_missing_one_is_listed() {
    let repo = npm_repo();
    let docs = resolve(&cfg(), &roots(repo.path(), DependencyHomes::default(), false));
    assert_eq!(
        docs.virtual_paths(),
        ["deps/npm/react@18.3.1/README.md", "deps/npm/react@18.3.1/docs/hooks.md"]
    );
    assert_eq!(docs.packages, 1);
    assert_eq!(
        reasons(&docs),
        [("left-pad".to_string(), REASON_NOT_INSTALLED.to_string())]
    );
    let readme = &docs.files["deps/npm/react@18.3.1/README.md"];
    assert_eq!(readme.rel, "README.md");
    assert!(readme.package_dir.ends_with("node_modules/react"));
}

#[cfg(unix)]
#[test]
fn a_symlinked_package_pointing_out_of_its_root_is_refused_even_with_the_grant() {
    let repo = npm_repo();
    let elsewhere = tempfile::tempdir().expect("elsewhere");
    write(&elsewhere.path().join("package.json"), r#"{"version":"1.0.0"}"#);
    write(&elsewhere.path().join("README.md"), "private notes\n");
    std::os::unix::fs::symlink(elsewhere.path(), repo.path().join("node_modules/left-pad")).expect("symlink");
    for granted in [false, true] {
        let docs = resolve(&cfg(), &roots(repo.path(), DependencyHomes::default(), granted));
        assert!(
            docs.files.keys().all(|path| !path.contains("left-pad")),
            "granted={granted}: {:?}",
            docs.files.keys()
        );
        assert_eq!(
            reasons(&docs),
            [("left-pad".to_string(), REASON_ESCAPES_ROOT.to_string())]
        );
    }
}

#[cfg(unix)]
#[test]
fn an_in_tree_root_that_resolves_outside_the_repository_needs_the_grant() {
    let repo = tempfile::tempdir().expect("repo");
    write(&repo.path().join("package.json"), r#"{"dependencies":{"react":"^18"}}"#);
    let store = tempfile::tempdir().expect("modules elsewhere");
    write(&store.path().join("react/package.json"), r#"{"version":"18.3.1"}"#);
    write(&store.path().join("react/README.md"), "# React\n");
    std::os::unix::fs::symlink(store.path(), repo.path().join("node_modules")).expect("symlink");

    let denied = resolve(&cfg(), &roots(repo.path(), DependencyHomes::default(), false));
    assert!(denied.files.is_empty());
    assert_eq!(
        reasons(&denied),
        [("react".to_string(), REASON_NOT_GRANTED.to_string())]
    );

    let granted = resolve(&cfg(), &roots(repo.path(), DependencyHomes::default(), true));
    assert_eq!(granted.virtual_paths(), ["deps/npm/react@18.3.1/README.md"]);
}

/// A repository depending on `serde 1.0.200` from crates.io, with a cargo home holding it.
fn cargo_fixture() -> (tempfile::TempDir, tempfile::TempDir) {
    let repo = tempfile::tempdir().expect("repo");
    write(&repo.path().join("Cargo.toml"), "[package]\nname = \"app\"\n");
    write(
        &repo.path().join("Cargo.lock"),
        "[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"serde\"]\n\
         [[package]]\nname = \"serde\"\nversion = \"1.0.200\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );
    let home = tempfile::tempdir().expect("cargo home");
    let crate_dir = home
        .path()
        .join("registry/src/index.crates.io-6f17d22bba15001f/serde-1.0.200");
    write(&crate_dir.join("README.md"), "# Serde\n");
    write(&crate_dir.join("CHANGELOG.md"), "## 1.0.200\n");
    write(&crate_dir.join("src/lib.rs"), "//! serde\n");
    (repo, home)
}

#[test]
fn the_cargo_registry_outside_the_repository_is_read_only_under_the_grant() {
    let (repo, home) = cargo_fixture();
    let homes = DependencyHomes {
        cargo_home: Some(home.path().to_path_buf()),
        ..DependencyHomes::default()
    };

    let denied = resolve(&cfg(), &roots(repo.path(), homes.clone(), false));
    assert!(denied.files.is_empty(), "{:?}", denied.files.keys());
    assert_eq!(
        reasons(&denied),
        [("serde".to_string(), REASON_NOT_GRANTED.to_string())]
    );

    let granted = resolve(&cfg(), &roots(repo.path(), homes, true));
    assert_eq!(
        granted.virtual_paths(),
        [
            "deps/cargo/serde@1.0.200/CHANGELOG.md",
            "deps/cargo/serde@1.0.200/README.md"
        ]
    );
    assert!(granted.unavailable.is_empty());
}

#[test]
fn a_vendored_crate_inside_the_repository_needs_no_grant() {
    let (repo, _home) = cargo_fixture();
    write(
        &repo.path().join("vendor/serde/Cargo.toml"),
        "[package]\nname = \"serde\"\nversion = \"1.0.200\"\n",
    );
    write(&repo.path().join("vendor/serde/README.md"), "# vendored serde\n");
    let docs = resolve(&cfg(), &roots(repo.path(), DependencyHomes::default(), false));
    assert_eq!(docs.virtual_paths(), ["deps/cargo/serde@1.0.200/README.md"]);
}

#[test]
fn a_go_module_is_read_from_the_module_cache_under_its_escaped_path() {
    let repo = tempfile::tempdir().expect("repo");
    write(
        &repo.path().join("go.mod"),
        "module example.com/app\n\nrequire github.com/BurntSushi/toml v1.3.2\n",
    );
    let cache = tempfile::tempdir().expect("module cache");
    write(
        &cache.path().join("github.com/!burnt!sushi/toml@v1.3.2/README.md"),
        "# TOML\n",
    );
    let homes = DependencyHomes {
        go_mod_cache: Some(cache.path().to_path_buf()),
        ..DependencyHomes::default()
    };
    let docs = resolve(&cfg(), &roots(repo.path(), homes, true));
    assert_eq!(
        docs.virtual_paths(),
        ["deps/go/github.com/BurntSushi/toml@v1.3.2/README.md"]
    );
}

#[test]
fn a_python_distribution_in_the_repository_venv_contributes_its_metadata() {
    let repo = tempfile::tempdir().expect("repo");
    write(
        &repo.path().join("pyproject.toml"),
        "[project]\nname = \"app\"\ndependencies = [\"Requests>=2\"]\n",
    );
    let site = repo.path().join(".venv/lib/python3.12/site-packages");
    write(
        &site.join("requests-2.32.3.dist-info/METADATA"),
        "Metadata-Version: 2.1\nName: requests\n\n# Requests\n",
    );
    write(&site.join("requests/__init__.py"), "");
    let docs = resolve(&cfg(), &roots(repo.path(), DependencyHomes::default(), false));
    assert_eq!(docs.virtual_paths(), ["deps/pypi/requests@2.32.3/METADATA"]);
    assert_eq!(
        docs.files["deps/pypi/requests@2.32.3/METADATA"].mime,
        Some("text/plain")
    );
}

#[test]
fn packages_past_max_packages_are_listed_not_indexed() {
    let repo = npm_repo();
    let other = repo.path().join("node_modules/left-pad");
    write(&other.join("package.json"), r#"{"version":"1.3.0"}"#);
    write(&other.join("README.md"), "# left-pad\n");
    let capped = DependenciesConfig {
        max_packages: 1,
        ..cfg()
    };
    let docs = resolve(&capped, &roots(repo.path(), DependencyHomes::default(), false));
    assert_eq!(docs.packages, 1);
    assert_eq!(docs.virtual_paths(), ["deps/npm/left-pad@1.3.0/README.md"]);
    assert_eq!(
        reasons(&docs),
        [("react".to_string(), REASON_OVER_MAX_PACKAGES.to_string())]
    );
}

#[test]
fn the_unavailable_record_round_trips_and_is_removed_when_empty() {
    let dir = tempfile::tempdir().expect("dir");
    let docs = DepDocs {
        unavailable: vec![DepUnavailable::new(
            Ecosystem::Npm,
            "left-pad",
            None,
            REASON_NOT_INSTALLED,
        )],
        ..DepDocs::default()
    };
    assert_eq!(record(dir.path(), true, Some(&docs)).len(), 1);
    assert_eq!(read_unavailable(dir.path()), docs.unavailable);
    assert!(record(dir.path(), true, None).is_empty());
    assert!(read_unavailable(dir.path()).is_empty());
    record(dir.path(), false, Some(&docs));
    assert!(read_unavailable(dir.path()).is_empty(), "another view records nothing");
}

#[test]
fn only_a_root_manifest_change_re_resolves_and_only_when_enabled() {
    let root = Path::new("/repo");
    let mut config = Config::with_defaults();
    let touched = [root.join("Cargo.lock")];
    assert!(!manifest_touched(root, &config, &touched), "off by default");
    config.documents.dependencies.enabled = true;
    assert_eq!(manifest_touched(root, &config, &touched), cfg!(feature = "documents"));
    assert!(!manifest_touched(root, &config, &[root.join("src/Cargo.lock")]));
    assert!(!manifest_touched(root, &config, &[root.join("README.md")]));
}
