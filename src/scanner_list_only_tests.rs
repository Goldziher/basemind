use std::path::Path;

use crate::config::Config;
use crate::config::list_only::{ListKind, ListedEntry};
use crate::scanner::{EmbedMode, ScanSource, scan};
use crate::scanner_file::OPENED;
use crate::store::{Store, VIEW_WORKING};

const LISTED: &[(&str, ListKind)] = &[
    ("Cargo.lock", ListKind::Lockfile),
    ("infra/prod/.terraform.lock.hcl", ListKind::Lockfile),
    ("web/pnpm-lock.yaml", ListKind::Lockfile),
    ("captures/session.pcap", ListKind::Data),
    ("dist-src/bundle.tar.gz", ListKind::Archive),
    ("assets/font.woff2", ListKind::Binary),
];

fn config(root: &Path) -> Config {
    let mut config = crate::config::default_for_root(root);
    config.documents.embed = false;
    config.code_search.embed = false;
    config
}

fn write(root: &Path, rel: &str, body: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn workspace() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "src/main.rs", b"fn main() {}\n");
    write(tmp.path(), "notes.md", b"# notes\n\nplain prose\n");
    for (rel, _) in LISTED {
        write(tmp.path(), rel, b"LISTED-CONTENT-MARKER");
    }
    tmp
}

fn run_scan(root: &Path, config: &Config) -> Store {
    crate::store::init_isolated_cache();
    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        config,
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
        crate::scanner_vectors::test_retrieval(),
    )
    .unwrap();
    store
}

fn opened_under(root: &Path) -> Vec<String> {
    OPENED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter_map(|path| path.strip_prefix(root).ok())
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .collect()
}

fn listed(store: &Store) -> Vec<(String, ListKind)> {
    let mut rows: Vec<(String, ListKind)> = store
        .index
        .listed
        .iter()
        .map(|(path, entry)| (path.to_str_lossy().into_owned(), entry.kind))
        .collect();
    rows.sort();
    rows
}

/// A list-only file is recorded from its stat and never opened: the fake-reader hook of
/// `read_working_tree` sees the indexed files and none of the listed ones.
#[test]
fn a_scan_lists_list_only_files_without_opening_them() {
    let tmp = workspace();
    let store = run_scan(tmp.path(), &config(tmp.path()));

    let mut expected: Vec<(String, ListKind)> = LISTED.iter().map(|(rel, kind)| ((*rel).to_string(), *kind)).collect();
    expected.sort();
    assert_eq!(listed(&store), expected);

    let opened = opened_under(tmp.path());
    for (rel, _) in LISTED {
        assert!(
            !opened.iter().any(|o| o == rel),
            "{rel} must never be opened: {opened:?}"
        );
    }
    assert!(
        opened.iter().any(|o| o == "src/main.rs"),
        "the reader was exercised: {opened:?}"
    );
}

#[test]
fn listed_files_are_in_no_index_tier_and_carry_their_stat() {
    let tmp = workspace();
    let store = run_scan(tmp.path(), &config(tmp.path()));

    for (rel, _) in LISTED {
        assert!(store.lookup(rel).is_none(), "{rel} is not code-mapped");
        assert!(store.lookup_doc(rel).is_none(), "{rel} is not a document");
    }
    assert!(store.lookup("src/main.rs").is_some());
    let entry: &ListedEntry = store.index.listed.get(bstr::BStr::new("Cargo.lock")).expect("listed");
    assert_eq!(entry.size_bytes, b"LISTED-CONTENT-MARKER".len() as u64);
    assert_ne!(entry.mtime, 0);
}

/// Each full pass replaces the recorded set: a deleted file leaves it.
#[test]
fn a_rescan_drops_deleted_files() {
    let tmp = workspace();
    let config = config(tmp.path());
    let _ = run_scan(tmp.path(), &config);

    std::fs::remove_file(tmp.path().join("Cargo.lock")).unwrap();
    let store = run_scan(tmp.path(), &config);

    let paths: Vec<String> = listed(&store).into_iter().map(|(path, _)| path).collect();
    assert_eq!(paths.len(), LISTED.len() - 1, "{paths:?}");
    assert!(!paths.iter().any(|p| p == "Cargo.lock"), "{paths:?}");
}

/// A `!` entry takes a file back out of the list, so it is extracted like any other.
#[test]
fn a_negated_entry_indexes_a_file_the_list_would_catch() {
    let tmp = workspace();
    write(
        tmp.path(),
        "generated/skip.rs",
        b"fn skipped() {}
",
    );
    write(tmp.path(), "generated/keep.rs", b"fn kept() {}\n");
    let mut config = config(tmp.path());
    config.scan.list_only.push("generated/".to_string());
    config.scan.list_only.push("!generated/keep.rs".to_string());
    let store = run_scan(tmp.path(), &config);

    let paths: Vec<String> = listed(&store).into_iter().map(|(path, _)| path).collect();
    assert!(paths.iter().any(|p| p == "generated/skip.rs"), "{paths:?}");
    assert!(!paths.iter().any(|p| p == "generated/keep.rs"), "{paths:?}");
    assert!(store.lookup("generated/keep.rs").is_some());
    assert!(store.lookup("generated/skip.rs").is_none());
    assert!(!opened_under(tmp.path()).iter().any(|o| o == "generated/skip.rs"));
}

/// `[scan] exclude` and the floor outrank the list: a file they drop is not even listed.
#[test]
fn excluded_files_are_not_listed() {
    let tmp = workspace();
    write(tmp.path(), "secrets/server.pem.zip", b"x");
    write(tmp.path(), "vendor/lib/Cargo.lock", b"x");
    let mut config = config(tmp.path());
    config.scan.exclude.push("secrets".to_string());
    let store = run_scan(tmp.path(), &config);

    let paths: Vec<String> = listed(&store).into_iter().map(|(path, _)| path).collect();
    assert!(!paths.iter().any(|p| p.starts_with("secrets/")), "{paths:?}");
    assert!(!paths.iter().any(|p| p.starts_with("vendor/")), "{paths:?}");
}

#[test]
fn an_empty_list_lists_nothing() {
    let tmp = workspace();
    let mut config = config(tmp.path());
    config.scan.list_only.clear();
    let store = run_scan(tmp.path(), &config);
    assert!(store.index.listed.is_empty());
}

/// Credential material and floor trees are never listed, and a listed path is no extraction
/// candidate in any pass.
#[test]
fn the_filters_list_only_what_the_floor_admits() {
    let config = crate::config::default_for_root(Path::new("."));
    let filters = crate::scanner_filter::Filters::build(&config, Vec::new(), crate::scanner_vectors::test_retrieval())
        .expect("filters");

    assert_eq!(filters.listed_kind("pkg/pnpm-lock.yaml"), Some(ListKind::Lockfile));
    assert!(!filters.allows("pkg/pnpm-lock.yaml"));
    assert!(!filters.allows_in_extra_root("pkg/pnpm-lock.yaml"));
    assert_eq!(
        filters.listed_kind(".ssh/backup.zip"),
        None,
        "credential trees stay invisible"
    );
    assert_eq!(filters.listed_kind("node_modules/x/yarn.lock"), None);
    assert_eq!(filters.listed_kind("src/main.rs"), None);
    assert!(filters.allows("src/main.rs"));
}
