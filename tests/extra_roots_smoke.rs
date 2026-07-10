//! `scan.extra_roots` — indexing directories outside the repository root (issue #34).
//!
//! Extra-root files are keyed by their **absolute** path (repo files stay repo-relative), so the
//! two namespaces never collide, and the code map (symbols, outlines, references) resolves across
//! the boundary. These tests mirror the end-to-end CLI behavior against the public query API.
//!
//! The external key is the file's absolute path in forward-slash form — a leading `/` on POSIX,
//! a drive prefix (`C:/…`) on Windows — so the suite runs on both.

use std::fs;
use std::path::PathBuf;

use basemind::config::ConfigV1;
use basemind::scanner::{ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};
use tempfile::TempDir;

/// A repo tempdir plus a *sibling* external dir (outside the repo root), wired into config.
fn repo_with_external() -> (TempDir, TempDir, ConfigV1) {
    let repo = tempfile::tempdir().expect("repo tempdir");
    let ext = tempfile::tempdir().expect("external tempdir");

    fs::write(
        repo.path().join("main.rs"),
        b"fn main() {\n    let _ = external_greet();\n    shared_helper();\n}\n",
    )
    .unwrap();

    fs::create_dir_all(ext.path().join("pkg")).unwrap();
    fs::write(
        ext.path().join("pkg/lib.rs"),
        b"pub fn external_greet() { shared_helper(); }\npub fn shared_helper() {}\n",
    )
    .unwrap();

    let mut cfg = ConfigV1::with_defaults();
    cfg.scan.extra_roots = vec![ext.path().to_path_buf()];
    (repo, ext, cfg)
}

fn abs_key(dir: &TempDir, rel: &str) -> String {
    let canonical = fs::canonicalize(dir.path()).unwrap();
    let key = canonical.join(rel).to_str().unwrap().to_string();
    #[cfg(windows)]
    let key = key.replace('\\', "/");
    key
}

#[test]
fn extra_root_files_indexed_under_absolute_keys() {
    let (repo, ext, cfg) = repo_with_external();
    let mut store = Store::open(repo.path(), VIEW_WORKING).unwrap();
    scan(
        repo.path(),
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .unwrap();

    assert!(store.lookup("main.rs").is_some(), "repo file keyed relative");

    let ext_key = abs_key(&ext, "pkg/lib.rs");
    assert!(
        std::path::Path::new(&ext_key).is_absolute(),
        "external key must be absolute, got {ext_key}"
    );
    let entry = store
        .lookup(ext_key.as_bytes())
        .unwrap_or_else(|| panic!("external file indexed under absolute key {ext_key}"));
    assert_eq!(entry.language, "rust");
    assert!(
        store.lookup("pkg/lib.rs").is_none(),
        "external file must not be indexed under a repo-relative key"
    );
}

#[test]
fn search_symbols_returns_external_symbol_with_absolute_path() {
    let (repo, ext, cfg) = repo_with_external();
    let mut store = Store::open(repo.path(), VIEW_WORKING).unwrap();
    scan(
        repo.path(),
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .unwrap();

    let hits = basemind::query::search_symbols(&store, "external_greet", None).unwrap();
    assert_eq!(hits.len(), 1, "external_greet found exactly once");
    assert_eq!(
        hits[0].path.as_str(),
        Some(abs_key(&ext, "pkg/lib.rs").as_str()),
        "hit carries the external file's absolute path"
    );
}

#[test]
fn outline_and_calls_resolve_for_external_file() {
    let (repo, ext, cfg) = repo_with_external();
    let mut store = Store::open(repo.path(), VIEW_WORKING).unwrap();
    scan(
        repo.path(),
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .unwrap();

    let ext_key = abs_key(&ext, "pkg/lib.rs");

    let l1 = basemind::query::file_outline(&store, ext_key.as_bytes()).unwrap();
    let names: Vec<&str> = l1.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"external_greet") && names.contains(&"shared_helper"));

    let l2 = basemind::query::file_outline_l2(&store, ext_key.as_bytes(), repo.path()).unwrap();
    assert!(
        l2.calls.iter().any(|c| c.callee == "shared_helper"),
        "external file's call to shared_helper is indexed (feeds cross-root find_references)"
    );
}

#[test]
fn removing_extra_root_prunes_external_files() {
    let (repo, ext, cfg) = repo_with_external();
    let mut store = Store::open(repo.path(), VIEW_WORKING).unwrap();
    scan(
        repo.path(),
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .unwrap();
    let ext_key = abs_key(&ext, "pkg/lib.rs");
    assert!(store.lookup(ext_key.as_bytes()).is_some());

    let mut cfg2 = ConfigV1::with_defaults();
    cfg2.scan.extra_roots = Vec::new();
    scan(
        repo.path(),
        &mut store,
        &cfg2,
        ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .unwrap();
    assert!(
        store.lookup(ext_key.as_bytes()).is_none(),
        "external key pruned after its root was removed from config"
    );
    assert!(store.lookup("main.rs").is_some(), "repo file survives");
}

#[test]
fn missing_and_inside_repo_extra_roots_are_skipped_without_failing() {
    let (repo, ext, mut cfg) = repo_with_external();
    let inside = repo.path().join("subdir");
    fs::create_dir_all(&inside).unwrap();
    fs::write(inside.join("in.rs"), b"pub fn inside() {}\n").unwrap();
    cfg.scan.extra_roots = vec![
        ext.path().to_path_buf(),
        PathBuf::from("/this/does/not/exist"),
        inside.clone(),
    ];

    let mut store = Store::open(repo.path(), VIEW_WORKING).unwrap();
    scan(
        repo.path(),
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
    )
    .unwrap();

    assert!(store.lookup(abs_key(&ext, "pkg/lib.rs").as_bytes()).is_some());
    assert!(store.lookup("subdir/in.rs").is_some());
    assert!(
        store.lookup(abs_key(&repo, "subdir/in.rs").as_bytes()).is_none(),
        "inside-repo extra_root must not double-index under an absolute key"
    );
}
