//! End-to-end checks for the scan-scoping config: `[languages]` overrides, `[scan]` include /
//! exclude / `floor_allow`, `extra_roots` matching, `[documents]` scoping, and config-digest
//! staleness. Each test scans a tempdir repo through the public scanner API.
//!
//! Embedding stays off throughout (no ONNX model is needed), so vector-row purging is covered at
//! the unit level in `scanner_policy` / `scanner_docs` / `scanner_code` against a real LanceDB
//! table instead.

use std::fs;
use std::path::Path;

use basemind::config::{self, ConfigV1, LanguageConfig};
use basemind::scanner::{EmbedMode, ScanReport, ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};
use tempfile::TempDir;

fn repo() -> (TempDir, ConfigV1) {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.embed = false;
    cfg.code_search.embed = false;
    (dir, cfg)
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn run_scan(root: &Path, store: &mut Store, cfg: &ConfigV1) -> ScanReport {
    scan(root, store, cfg, ScanSource::WorkingTree, EmbedMode::Inline).expect("scan")
}

fn language(extensions: &[&str], filenames: &[&str], enabled: bool) -> LanguageConfig {
    LanguageConfig {
        enabled,
        extensions: extensions.iter().map(|s| (*s).to_string()).collect(),
        filenames: filenames.iter().map(|s| (*s).to_string()).collect(),
        preload: false,
    }
}

const PY: &str = "def mako_render(context):\n    return context\n";

#[test]
fn language_extension_and_filename_overrides_index_unrecognised_paths() {
    let (dir, mut cfg) = repo();
    let root = dir.path();
    write(root, "tpl/page.mako", PY);
    write(root, "pkg/BUILD.in", "def build_in_rule():\n    pass\n");
    let mut store = Store::open(root, VIEW_WORKING).unwrap();

    run_scan(root, &mut store, &cfg);
    assert!(
        store.lookup("tpl/page.mako").is_none(),
        "unrecognised suffix is not code"
    );
    assert!(store.lookup("pkg/BUILD.in").is_none());

    cfg.languages
        .insert("python".to_string(), language(&[".mako"], &["BUILD.in"], true));
    let report = run_scan(root, &mut store, &cfg);
    assert_eq!(
        report.stats.updated, 2,
        "both paths are parsed once the override exists"
    );
    assert_eq!(
        store.lookup("tpl/page.mako").expect("mapped by extension").language,
        "python"
    );
    assert_eq!(
        store.lookup("pkg/BUILD.in").expect("mapped by file name").language,
        "python"
    );

    let hits = basemind::query::search_symbols(&store, "mako_render", None).unwrap();
    assert_eq!(hits.len(), 1, "symbols from the remapped file are searchable");
    assert_eq!(hits[0].path.as_str(), Some("tpl/page.mako"));
}

#[test]
fn remapping_a_path_to_another_grammar_reextracts_unchanged_bytes() {
    let (dir, mut cfg) = repo();
    let root = dir.path();
    write(root, "x.weird", "pub fn remap_target() {}\n");
    let mut store = Store::open(root, VIEW_WORKING).unwrap();

    cfg.languages
        .insert("rust".to_string(), language(&[".weird"], &[], true));
    run_scan(root, &mut store, &cfg);
    assert_eq!(store.lookup("x.weird").unwrap().language, "rust");
    assert_eq!(
        basemind::query::file_outline(&store, "x.weird").unwrap().language,
        "rust"
    );

    cfg.languages.remove("rust");
    cfg.languages
        .insert("python".to_string(), language(&[".weird"], &[], true));
    let report = run_scan(root, &mut store, &cfg);
    assert_eq!(report.stats.updated, 1, "same bytes, new grammar: not Unchanged");
    assert_eq!(store.lookup("x.weird").unwrap().language, "python");
    assert_eq!(
        basemind::query::file_outline(&store, "x.weird").unwrap().language,
        "python",
        "the shared content-addressed outline blob is rewritten for the new grammar"
    );
}

#[test]
fn disabling_a_language_drops_its_files_from_the_index() {
    let (dir, mut cfg) = repo();
    let root = dir.path();
    write(root, "a.rs", "pub fn alpha() {}\n");
    write(root, "b.py", "def beta():\n    pass\n");
    let mut store = Store::open(root, VIEW_WORKING).unwrap();

    run_scan(root, &mut store, &cfg);
    assert!(store.lookup("a.rs").is_some() && store.lookup("b.py").is_some());

    cfg.languages.insert("rust".to_string(), language(&[], &[], false));
    let report = run_scan(root, &mut store, &cfg);
    assert!(store.lookup("a.rs").is_none(), "disabled language is purged");
    assert!(store.lookup("b.py").is_some(), "other languages are untouched");
    assert_eq!(report.stats.removed, 1);
    assert!(
        basemind::query::search_symbols(&store, "alpha", None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn unknown_language_key_is_a_config_error_with_suggestions() {
    let err = config::parse_str("\"$schema\" = \"v1\"\n[languages.pythn]\nenabled = false\n")
        .expect_err("typo in grammar name");
    let msg = err.to_string();
    assert!(msg.contains("[languages.pythn]"), "{msg}");
    assert!(msg.contains("python"), "suggests near matches: {msg}");
}

#[test]
fn empty_scan_include_and_invalid_globs_are_rejected_at_load() {
    let err = config::parse_str("\"$schema\" = \"v1\"\n[scan]\ninclude = []\n").expect_err("empty include");
    assert!(err.to_string().contains("include"), "{err}");

    let err = config::parse_str("\"$schema\" = \"v1\"\n[documents]\nembed_exclude = [\"a/[\"]\n")
        .expect_err("unbalanced glob class");
    assert!(err.to_string().contains("embed_exclude"), "{err}");

    let cfg = config::parse_str("\"$schema\" = \"v1\"\n[scan]\ninclude = [\"src\"]\n").expect("bare name is valid");
    assert_eq!(cfg.scan.include, vec!["src".to_string()]);
}

#[test]
fn bare_exclude_name_excludes_every_directory_of_that_name() {
    let (dir, mut cfg) = repo();
    let root = dir.path();
    write(root, "generated/schema.rs", "pub fn g1() {}\n");
    write(root, "src/generated/deep/types.rs", "pub fn g2() {}\n");
    write(root, "src/generated_helpers.rs", "pub fn kept_similar_name() {}\n");
    write(root, "src/keep.rs", "pub fn kept() {}\n");
    cfg.scan.exclude = vec!["generated".to_string()];
    let mut store = Store::open(root, VIEW_WORKING).unwrap();

    run_scan(root, &mut store, &cfg);
    assert!(store.lookup("generated/schema.rs").is_none());
    assert!(store.lookup("src/generated/deep/types.rs").is_none());
    assert!(store.lookup("src/keep.rs").is_some());
    assert!(
        store.lookup("src/generated_helpers.rs").is_some(),
        "a bare name matches whole path segments only"
    );
}

#[test]
fn floor_allow_lets_a_floor_directory_be_indexed() {
    let (dir, mut cfg) = repo();
    let root = dir.path();
    write(root, "build/gen.rs", "pub fn generated_by_build() {}\n");
    write(root, "out/other.rs", "pub fn stays_out() {}\n");
    write(root, "src/lib.rs", "pub fn lib() {}\n");
    let mut store = Store::open(root, VIEW_WORKING).unwrap();

    run_scan(root, &mut store, &cfg);
    assert!(
        store.lookup("build/gen.rs").is_none(),
        "floor excludes build/ by default"
    );

    cfg.scan.floor_allow = vec!["build".to_string(), "not-a-floor-entry".to_string()];
    run_scan(root, &mut store, &cfg);
    assert!(store.lookup("build/gen.rs").is_some(), "floor_allow removed the entry");
    assert!(
        store.lookup("out/other.rs").is_none(),
        "other floor entries still apply"
    );
}

#[test]
fn extra_root_inside_a_floor_named_directory_is_still_indexed_and_matches_relatively() {
    basemind::store::init_isolated_cache();
    basemind::scanner::allow_extra_roots(true);
    let repo = tempfile::tempdir().unwrap();
    let outer = tempfile::tempdir().unwrap();
    let ext = outer.path().join("build").join("vendor").join("ext");
    write(&ext, "pkg/lib.rs", "pub fn external_in_build() {}\n");
    write(&ext, "pkg/skip/gen.rs", "pub fn external_skipped() {}\n");
    write(repo.path(), "main.rs", "fn main() {}\n");

    let mut cfg = ConfigV1::with_defaults();
    cfg.scan.extra_roots = vec![ext.clone()];
    cfg.scan.exclude.push("pkg/skip".to_string());
    let mut store = Store::open(repo.path(), VIEW_WORKING).unwrap();
    run_scan(repo.path(), &mut store, &cfg);

    let canonical = fs::canonicalize(&ext).unwrap();
    let key = |rel: &str| canonical.join(rel).to_str().unwrap().replace('\\', "/");
    assert!(
        store.lookup(key("pkg/lib.rs").as_bytes()).is_some(),
        "an absolute prefix containing build/ and vendor/ must not drop the whole root"
    );
    assert!(
        store.lookup(key("pkg/skip/gen.rs").as_bytes()).is_none(),
        "a root-relative exclude applies inside the extra root"
    );
}

#[test]
fn embed_policy_is_recorded_and_tracks_config_changes() {
    let (dir, mut cfg) = repo();
    let root = dir.path();
    write(root, "a.rs", "pub fn a() {}\n");
    let mut store = Store::open(root, VIEW_WORKING).unwrap();

    assert!(store.index.embed_policy.is_empty());
    run_scan(root, &mut store, &cfg);
    let first = store.index.embed_policy.clone();
    assert!(!first.is_empty(), "a complete full scan records the embed policy");

    run_scan(root, &mut store, &cfg);
    assert_eq!(
        store.index.embed_policy, first,
        "unchanged config keeps the recorded policy"
    );

    cfg.documents.embed_exclude = vec!["docs/**".to_string()];
    run_scan(root, &mut store, &cfg);
    assert_ne!(
        store.index.embed_policy, first,
        "embed scope change is reconciled and re-recorded"
    );
}

#[cfg(feature = "code-search")]
mod code_chunks {
    use super::*;

    fn big_rust_file() -> String {
        let mut body = String::from("pub fn big_function() {\n");
        for i in 0..200 {
            body.push_str(&format!(
                "    let value_{i} = {i} * 2 + compute_something_long_{i}();\n"
            ));
        }
        body.push_str("}\n");
        body
    }

    fn chunk_count(store: &Store, rel: &str) -> usize {
        let entry = store.lookup(rel).expect("indexed");
        store
            .read_chunks_by_hex(&entry.hash_hex)
            .unwrap()
            .expect("chunk sidecar")
            .chunks
            .len()
    }

    #[test]
    fn changing_code_search_chunk_size_rechunks_unchanged_files() {
        let (dir, mut cfg) = repo();
        let root = dir.path();
        write(root, "big.rs", &big_rust_file());
        let mut store = Store::open(root, VIEW_WORKING).unwrap();

        cfg.code_search.max_characters = 4000;
        cfg.code_search.overlap = 100;
        run_scan(root, &mut store, &cfg);
        let coarse = chunk_count(&store, "big.rs");

        let again = run_scan(root, &mut store, &cfg);
        assert_eq!(again.stats.skipped_unchanged, 1, "same config: the file is Unchanged");

        cfg.code_search.max_characters = 200;
        cfg.code_search.overlap = 20;
        let report = run_scan(root, &mut store, &cfg);
        assert_eq!(
            report.stats.skipped_unchanged, 0,
            "digest change defeats the unchanged fast path"
        );
        assert!(
            chunk_count(&store, "big.rs") > coarse,
            "smaller max_characters yields more chunks ({coarse} before)"
        );
    }
}

#[cfg(feature = "documents")]
mod documents {
    use super::*;
    use basemind::scanner::{CollectObserver, FileStatus, scan_with_observer};

    /// `.eml` must stay outside the tree-sitter registry for these tests to reach the document tier.
    fn csv_is_a_document() {
        assert!(
            basemind::lang::detect(Path::new("x.eml")).is_none(),
            "x.eml became a code language; pick another non-code document extension for these tests"
        );
    }

    /// A plain-text RFC 822 message with `rows` body paragraphs.
    fn csv(rows: usize) -> String {
        let body: String = (0..rows)
            .map(|i| format!("row{i} alpha beta gamma delta {i} epsilon zeta eta theta {i}\n\n"))
            .collect();
        format!("From: a@example.com\nTo: b@example.com\nSubject: report\nContent-Type: text/plain\n\n{body}")
    }

    fn doc_cfg() -> (TempDir, ConfigV1) {
        let (dir, mut cfg) = repo();
        cfg.documents.embed = false;
        cfg.code_search.enabled = false;
        (dir, cfg)
    }

    #[test]
    fn documents_include_exclude_scope_which_files_become_documents() {
        csv_is_a_document();
        let (dir, mut cfg) = doc_cfg();
        let root = dir.path();
        write(root, "docs/a.eml", &csv(5));
        write(root, "docs/draft/c.eml", &csv(5));
        write(root, "data/b.eml", &csv(5));
        let mut store = Store::open(root, VIEW_WORKING).unwrap();

        run_scan(root, &mut store, &cfg);
        for rel in ["docs/a.eml", "docs/draft/c.eml", "data/b.eml"] {
            assert!(store.lookup_doc(rel).is_some(), "default indexes every document: {rel}");
        }

        cfg.documents.include = vec!["docs".to_string()];
        cfg.documents.exclude = vec!["draft".to_string()];
        run_scan(root, &mut store, &cfg);
        assert!(store.lookup_doc("docs/a.eml").is_some());
        assert!(store.lookup_doc("data/b.eml").is_none(), "outside documents.include");
        assert!(store.lookup_doc("docs/draft/c.eml").is_none(), "exclude beats include");
    }

    #[test]
    fn extension_denylist_accepts_leading_dot_and_any_case() {
        csv_is_a_document();
        let (dir, mut cfg) = doc_cfg();
        let root = dir.path();
        write(root, "a.eml", &csv(3));
        cfg.documents.extension_denylist = vec![".EML".to_string()];
        let mut store = Store::open(root, VIEW_WORKING).unwrap();
        run_scan(root, &mut store, &cfg);
        assert!(store.lookup_doc("a.eml").is_none());
    }

    #[test]
    fn documents_max_file_bytes_is_independent_of_scan_max_file_bytes() {
        csv_is_a_document();
        let (dir, mut cfg) = doc_cfg();
        let root = dir.path();
        write(root, "big.eml", &csv(4000));
        let size = fs::metadata(root.join("big.eml")).unwrap().len();
        cfg.scan.max_file_bytes = 1024;
        cfg.documents.max_file_bytes = size + 1;
        let mut store = Store::open(root, VIEW_WORKING).unwrap();

        let report = run_scan(root, &mut store, &cfg);
        assert!(
            store.lookup_doc("big.eml").is_some(),
            "a document over scan.max_file_bytes is governed by documents.max_file_bytes"
        );
        assert_eq!(report.stats.skipped_too_large, 0);

        cfg.documents.max_file_bytes = 2048;
        let mut observer = CollectObserver::default();
        let report = scan_with_observer(
            root,
            &mut store,
            &cfg,
            ScanSource::WorkingTree,
            EmbedMode::Inline,
            &basemind::scanner::ScanCancel::new(),
            &mut observer,
        )
        .unwrap();
        assert_eq!(
            report.stats.skipped_too_large, 1,
            "over the document cap: counted as too large"
        );
        assert!(store.lookup_doc("big.eml").is_none());
        assert!(
            observer
                .results()
                .iter()
                .any(|r| matches!(r.status, FileStatus::SkippedTooLarge { .. })),
            "the skip is reported per file"
        );
    }

    #[test]
    fn changing_document_chunk_size_reextracts_unchanged_documents() {
        csv_is_a_document();
        let (dir, mut cfg) = doc_cfg();
        let root = dir.path();
        write(root, "table.eml", &csv(300));
        let mut store = Store::open(root, VIEW_WORKING).unwrap();

        // Which settings the stored blob was extracted under. (Chunk counts are not asserted: xberg
        // sizes chunks from the embedding preset when one is set, so `max_characters` alone need not
        // change them.)
        let digest_of = |store: &Store| {
            let entry = store.lookup_doc("table.eml").expect("tracked").clone();
            store
                .read_doc_by_hex(&entry.hash_hex)
                .unwrap()
                .expect("doc blob")
                .config_digest
        };

        cfg.documents.max_characters = 4000;
        cfg.documents.overlap = 100;
        run_scan(root, &mut store, &cfg);
        let coarse = digest_of(&store);
        assert!(!coarse.is_empty(), "blobs are stamped with the extraction digest");

        let again = run_scan(root, &mut store, &cfg);
        assert_eq!(again.stats.skipped_unchanged, 1, "same config: Unchanged");

        cfg.documents.max_characters = 500;
        cfg.documents.overlap = 50;
        let report = run_scan(root, &mut store, &cfg);
        assert_eq!(report.stats.skipped_unchanged, 0, "digest change defeats the fast path");
        assert_eq!(
            report.stats.reused_doc_extraction, 0,
            "the cached blob is stale, so it is re-extracted"
        );
        assert_ne!(
            digest_of(&store),
            coarse,
            "the re-extracted blob carries the new digest"
        );
    }
}
