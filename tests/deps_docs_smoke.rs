//! Dependency documentation (#132, ADR-0049), end to end through the scanner: the documentation of
//! an installed dependency is indexed under its virtual path and found by `docs_search`'s `path`
//! glob with its package cited; what is not indexed is listed; nothing outside a package root or
//! outside the repository without the operator grant is read.
//!
//! Every package root here is a fixture: the operator's package caches are replaced through the
//! engine configuration, which is process-wide, so every test holds [`ENGINE`] and sets the caches
//! and the grant it needs. Plain `#[test]` only: opening LanceDB inside a tokio runtime panics.

#![cfg(feature = "documents")]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use basemind::config::ConfigV1;
use basemind::config::engine::ExtraRootsGrant;
use basemind::scanner::{EmbedMode, ScanReport, ScanSource, scan, scan_paths};
use basemind::store::{Store, VIEW_WORKING};

static ENGINE: Mutex<()> = Mutex::new(());

/// Hold the engine configuration with `cargo_home` as the cargo cache and the grant as given.
fn engine(cargo_home: Option<&Path>, granted: bool) -> MutexGuard<'static, ()> {
    let guard = ENGINE.lock().unwrap_or_else(PoisonError::into_inner);
    basemind::store::init_isolated_cache();
    basemind::config::engine::update(|engine| {
        engine.dependency_homes.cargo_home = cargo_home.map(Path::to_path_buf);
        engine.dependency_homes.go_mod_cache = None;
        engine.dependency_homes.virtual_env = None;
        engine.trust.dependency_roots = if granted {
            ExtraRootsGrant::Every
        } else {
            ExtraRootsGrant::Denied
        };
    });
    guard
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, text).expect("write");
}

fn config() -> ConfigV1 {
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.enabled = true;
    cfg.documents.embed = false;
    cfg.documents.dependencies.enabled = true;
    cfg
}

/// A repository whose `package.json` names `react` (installed, with documentation) and `left-pad`
/// (not installed). Contents are salted with the root so no test reuses another's blob.
fn npm_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("repo");
    let salt = repo.path().display().to_string();
    write(&repo.path().join("main.rs"), "fn main() {}\n");
    write(
        &repo.path().join("package.json"),
        r#"{"dependencies":{"react":"^18","left-pad":"^1"}}"#,
    );
    let react = repo.path().join("node_modules/react");
    write(&react.join("package.json"), r#"{"name":"react","version":"18.3.1"}"#);
    write(
        &react.join("README.md"),
        &format!("# React\n\nReact renders reconciled component trees ({salt}).\n"),
    );
    write(
        &react.join("docs/hooks.md"),
        &format!("# Hooks\n\nuseState keeps reconciled state between renders ({salt}).\n"),
    );
    write(&react.join("index.js"), "module.exports = {};\n");
    repo
}

fn full_scan(root: &Path, store: &mut Store, cfg: &ConfigV1) -> ScanReport {
    scan(
        root,
        store,
        cfg,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("scan")
}

fn virtual_docs(store: &Store) -> Vec<String> {
    let mut paths: Vec<String> = store
        .index
        .doc_files
        .keys()
        .map(|path| path.to_str_lossy().into_owned())
        .filter(|path| path.starts_with("deps/"))
        .collect();
    paths.sort();
    paths
}

fn unavailable(report: &ScanReport) -> Vec<(String, String)> {
    report
        .deps_unavailable
        .iter()
        .map(|dep| (dep.name.clone(), dep.reason.clone()))
        .collect()
}

#[test]
fn an_installed_dependency_is_indexed_searchable_by_path_and_cites_its_version() {
    let _engine = engine(None, false);
    let repo = npm_repo();
    let root = repo.path();
    let cfg = config();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    let report = full_scan(root, &mut store, &cfg);

    assert_eq!(
        virtual_docs(&store),
        ["deps/npm/react@18.3.1/README.md", "deps/npm/react@18.3.1/docs/hooks.md"]
    );
    assert!(store.lookup("main.rs").is_some(), "the repository still indexes");
    assert!(
        store.lookup_doc("node_modules/react/README.md").is_none(),
        "the package is read only through its virtual path"
    );
    assert_eq!(
        unavailable(&report),
        [("left-pad".to_string(), "not_installed".to_string())]
    );
    assert_eq!(
        basemind::deps_docs::read_unavailable(&store.basemind_dir),
        report.deps_unavailable,
        "index_status reads what the scan recorded"
    );

    let dim = 768;
    let docs = basemind::lance::docs::DocStore::shared_existing(
        &basemind::lance::docs::group_store_dir(root, &cfg.documents.embedding_preset, dim),
        dim,
        &cfg.documents.embedding_preset,
    )
    .expect("open")
    .expect("the scan wrote the store");
    let request = basemind::lance::docs_rank::DocsRequest {
        query: "reconciled".to_string(),
        mime_type: None,
        format: None,
        entity: None,
        keywords: None,
        path: Some("deps/npm/react@*/**".to_string()),
    };
    let filters = basemind::lance::docs_rank::Filters::parse(&request, None).expect("filters");
    let inputs = basemind::lance::docs_rank::RankInputs {
        store: Some(&docs),
        live: basemind::lance::docs_rank::LiveDocs::of_store(&store, filters.path.as_ref()),
        owed: 0,
        embed: Err("embeddings off".to_string()),
        rerank: None,
    };
    let ranking = basemind::lance::docs_rank::rank(&request.query, &filters, inputs).expect("rank");
    let page = basemind::lance::docs_page::page(
        Some(&docs),
        ranking,
        basemind::lance::docs_page::Paging {
            offset: 0,
            limit: 10,
            max_tokens: None,
            per_doc: basemind::lance::docs_page::PER_DOC_DEFAULT,
            context: false,
        },
    );
    let mut paths: Vec<&str> = page.hits.iter().map(|hit| hit.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        ["deps/npm/react@18.3.1/README.md", "deps/npm/react@18.3.1/docs/hooks.md"]
    );
    for hit in &page.hits {
        assert_eq!(hit.package.as_deref(), Some("react"), "{}", hit.path);
        assert_eq!(hit.package_version.as_deref(), Some("18.3.1"), "{}", hit.path);
    }

    let mut off = cfg.clone();
    off.documents.dependencies.enabled = false;
    let report = full_scan(root, &mut store, &off);
    assert!(virtual_docs(&store).is_empty(), "turning it off drops the documents");
    assert!(report.deps_unavailable.is_empty());
    assert!(basemind::deps_docs::read_unavailable(&store.basemind_dir).is_empty());
}

#[cfg(unix)]
#[test]
fn a_package_symlinked_out_of_node_modules_is_never_read() {
    let _engine = engine(None, true);
    let repo = npm_repo();
    let root = repo.path();
    let secrets = tempfile::tempdir().expect("secrets");
    write(&secrets.path().join("package.json"), r#"{"version":"1.0.0"}"#);
    write(&secrets.path().join("README.md"), "private key material\n");
    std::os::unix::fs::symlink(secrets.path(), root.join("node_modules/left-pad")).expect("symlink");

    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    let report = full_scan(root, &mut store, &config());

    assert!(
        virtual_docs(&store).iter().all(|path| !path.contains("left-pad")),
        "{:?}",
        virtual_docs(&store)
    );
    assert_eq!(
        unavailable(&report),
        [("left-pad".to_string(), "escapes_root".to_string())],
        "refused even under the grant"
    );
}

/// `serde 1.0.200` from crates.io, and a cargo home holding its source.
fn cargo_repo() -> (tempfile::TempDir, tempfile::TempDir) {
    let repo = tempfile::tempdir().expect("repo");
    let salt = repo.path().display().to_string();
    write(&repo.path().join("Cargo.toml"), "[package]\nname = \"app\"\n");
    write(
        &repo.path().join("Cargo.lock"),
        "[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"serde\"]\n\
         [[package]]\nname = \"serde\"\nversion = \"1.0.200\"\n\
         source = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );
    let home = tempfile::tempdir().expect("cargo home");
    write(
        &home
            .path()
            .join("registry/src/index.crates.io-6f17d22bba15001f/serde-1.0.200/README.md"),
        &format!("# Serde\n\nA serialization framework ({salt}).\n"),
    );
    (repo, home)
}

#[test]
fn the_cargo_registry_in_the_home_needs_the_operator_grant() {
    let (repo, home) = cargo_repo();
    let root = repo.path();

    let report = {
        let _engine = engine(Some(home.path()), false);
        let mut store = Store::open(root, VIEW_WORKING).expect("open");
        let report = full_scan(root, &mut store, &config());
        assert!(virtual_docs(&store).is_empty(), "{:?}", virtual_docs(&store));
        report
    };
    assert_eq!(unavailable(&report), [("serde".to_string(), "not_granted".to_string())]);

    let _engine = engine(Some(home.path()), true);
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    let report = full_scan(root, &mut store, &config());
    assert_eq!(virtual_docs(&store), ["deps/cargo/serde@1.0.200/README.md"]);
    assert!(report.deps_unavailable.is_empty());
}

/// A repository's own `basemind.toml` cannot name a package root: the setting does not exist, so the
/// file is refused, and enabling the feature alone reaches nothing outside the tree.
#[test]
fn a_repository_config_cannot_name_or_grant_a_package_root() {
    let (repo, home) = cargo_repo();
    let root = repo.path();
    let _engine = engine(Some(home.path()), false);

    let home_toml = home.path().display().to_string().replace('\\', "\\\\");
    write(
        &root.join("basemind.toml"),
        &format!("\"$schema\" = \"v1\"\n[documents.dependencies]\nenabled = true\nroots = [\"{home_toml}\"]\n"),
    );
    let refused = basemind::config::load(root);
    assert!(
        refused.as_ref().is_err_and(|error| error.to_string().contains("roots")),
        "{refused:?}"
    );

    write(
        &root.join("basemind.toml"),
        "\"$schema\" = \"v1\"\n[documents]\nembed = false\n[documents.dependencies]\nenabled = true\n",
    );
    let cfg = basemind::config::load(root).expect("enabling the feature is allowed");
    assert!(cfg.documents.dependencies.enabled);
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    let report = full_scan(root, &mut store, &cfg);
    assert!(virtual_docs(&store).is_empty(), "{:?}", virtual_docs(&store));
    assert_eq!(unavailable(&report), [("serde".to_string(), "not_granted".to_string())]);
}

#[test]
fn a_changed_manifest_re_resolves_the_dependencies_on_an_incremental_pass() {
    let _engine = engine(None, false);
    let repo = npm_repo();
    let root = repo.path();
    let cfg = config();
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    full_scan(root, &mut store, &cfg);

    let pad: PathBuf = root.join("node_modules/left-pad");
    write(&pad.join("package.json"), r#"{"version":"1.3.0"}"#);
    write(&pad.join("README.md"), &format!("# left-pad ({})\n", root.display()));
    write(&root.join("package.json"), r#"{"dependencies":{"left-pad":"^1"}}"#);

    let report = scan_paths(
        root,
        &mut store,
        &cfg,
        &[root.join("package.json")],
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("scan_paths");
    assert_eq!(virtual_docs(&store), ["deps/npm/left-pad@1.3.0/README.md"]);
    assert!(report.deps_unavailable.is_empty());
}
