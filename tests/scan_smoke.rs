use std::fs;

use basemind::config::ConfigV1;
use basemind::extract::SymbolKind;
use basemind::scanner::{FileStatus, scan, scan_paths};
use basemind::store::Store;
use tempfile::TempDir;

fn fresh_repo() -> (TempDir, ConfigV1) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = ConfigV1::with_defaults();
    (dir, cfg)
}

#[test]
fn scan_extracts_rust_symbols() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();

    fs::write(
        root.join("a.rs"),
        b"pub fn alpha() {}\npub struct Beta { x: i32 }\n",
    )
    .unwrap();

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    let report = scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    assert_eq!(report.stats.updated, 1);
    assert_eq!(report.stats.skipped_unchanged, 0);

    let entry = store.lookup("a.rs").expect("a.rs indexed");
    assert_eq!(entry.language, "rust");

    let hits = basemind::query::search_symbols(&store, "alpha", None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].symbol.kind, SymbolKind::Function);
    assert_eq!(hits[0].path.as_str(), Some("a.rs"));

    let hits = basemind::query::search_symbols(&store, "Beta", Some(SymbolKind::Struct)).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn scan_indexes_dynamic_language_without_override_queries() {
    // A file in a TSLP-supported language for which basemind ships no hand-written `.scm`
    // override now resolves through the TSLP `tags.scm` fallback (where one exists). For
    // formats with no tags.scm (e.g. JSON / YAML), the file still indexes but symbols stay
    // empty — exercised here with a `.json` file to keep the test focused on the negative
    // branch. Positive-branch coverage for kotlin / csharp lives in `tests/lang_fallback_smoke.rs`.
    let (dir, cfg) = fresh_repo();
    let root = dir.path();

    fs::write(root.join("data.json"), b"{ \"alpha\": 1 }\n").unwrap();

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    let report = scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    assert_eq!(report.stats.updated, 1, "json file should be processed");
    assert_eq!(report.stats.skipped_no_lang, 0, "json must not be skipped");

    let entry = store.lookup("data.json").expect("data.json indexed");
    assert_eq!(entry.language, "json", "language stored as TSLP pack name");

    // No tags.scm for JSON in TSLP — fallback misses, symbols stay empty, lookup chain
    // doesn't error.
    let hits = basemind::query::search_symbols(&store, "alpha", None).unwrap();
    assert!(hits.is_empty(), "json has no tags.scm; symbols stay empty");
}

#[test]
fn rescan_is_idempotent_and_uses_cache() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();

    fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").unwrap();

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    let first = scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    assert_eq!(first.stats.updated, 1);
    drop(store);

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    let second = scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    assert_eq!(second.stats.updated, 0);
    assert_eq!(second.stats.skipped_unchanged, 1);
}

#[test]
fn modifying_a_file_triggers_reextract() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();

    fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").unwrap();
    {
        let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
        scan(
            root,
            &mut store,
            &cfg,
            basemind::scanner::ScanSource::WorkingTree,
        )
        .unwrap();
    }
    fs::write(root.join("a.rs"), b"pub fn gamma() {}\n").unwrap();
    {
        let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
        let s = scan(
            root,
            &mut store,
            &cfg,
            basemind::scanner::ScanSource::WorkingTree,
        )
        .unwrap();
        assert_eq!(s.stats.updated, 1);
        let hits = basemind::query::search_symbols(&store, "gamma", None).unwrap();
        assert_eq!(hits.len(), 1);
        let hits = basemind::query::search_symbols(&store, "alpha", None).unwrap();
        assert!(hits.is_empty(), "old symbol should be gone");
    }
}

#[test]
fn removed_files_get_purged_from_index() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();

    fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").unwrap();
    fs::write(root.join("b.rs"), b"pub fn beta() {}\n").unwrap();
    {
        let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
        scan(
            root,
            &mut store,
            &cfg,
            basemind::scanner::ScanSource::WorkingTree,
        )
        .unwrap();
    }
    fs::remove_file(root.join("b.rs")).unwrap();
    {
        let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
        let s = scan(
            root,
            &mut store,
            &cfg,
            basemind::scanner::ScanSource::WorkingTree,
        )
        .unwrap();
        assert_eq!(s.stats.removed, 1);
        assert!(store.lookup("b.rs").is_none());
        assert!(store.lookup("a.rs").is_some());
    }
}

#[test]
fn skips_large_files() {
    let (dir, mut cfg) = fresh_repo();
    cfg.scan.max_file_bytes = 1024;
    let root = dir.path();

    let big = vec![b'x'; 4096];
    fs::write(root.join("big.rs"), &big).unwrap();

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    let s = scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    assert_eq!(s.stats.skipped_too_large, 1);
    assert!(store.lookup("big.rs").is_none());
}

#[test]
fn ignores_unknown_languages() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(root.join("weird.xyz"), b"data").unwrap();

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    let _report = scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    // `.xyz` is not in the tree-sitter-language-pack registry. Without the documents
    // feature it's counted as `skipped_no_lang`; with documents enabled the scanner
    // hands it to the doc tier where mime sniffing + chunking decide its fate (typically
    // `extract_failed` or `doc_indexed` with zero chunks). The invariant that holds in
    // both modes is that the file never lands in the code-tier index.
    #[cfg(not(feature = "documents"))]
    assert_eq!(_report.stats.skipped_no_lang, 1);
    assert!(store.lookup("weird.xyz").is_none());
}

#[test]
fn extracts_python() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(
        root.join("m.py"),
        b"import os\n\ndef foo(x):\n    return x\n\nclass Bar:\n    pass\n",
    )
    .unwrap();

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();

    let outline = basemind::query::file_outline(&store, "m.py").unwrap();
    assert_eq!(outline.language, "python");
    let names: Vec<&str> = outline.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"foo"));
    assert!(names.contains(&"Bar"));
    assert!(!outline.imports.is_empty());
}

#[test]
fn store_lock_prevents_concurrent_open() {
    let (dir, _cfg) = fresh_repo();
    let root = dir.path();
    let first = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    let err = Store::open(root, basemind::store::VIEW_WORKING)
        .err()
        .expect("second open must fail");
    assert!(matches!(err, basemind::store::StoreError::Locked(_)));
    drop(first);
    // After dropping, open succeeds again.
    Store::open(root, basemind::store::VIEW_WORKING).unwrap();
}

#[test]
fn scan_flags_files_with_syntax_errors() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    // Broken `fn x( {` plus a well-formed neighbor.
    fs::write(
        root.join("broken.rs"),
        b"pub fn ok_one() {}\n\npub fn broken( {\n    let x = ;\n}\n",
    )
    .unwrap();

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    let report = scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    assert_eq!(report.stats.updated, 1);
    assert_eq!(
        report.stats.updated_with_warnings, 1,
        "should flag the file as having parse errors"
    );

    let row = report
        .results
        .iter()
        .find(|r| r.path == "broken.rs")
        .expect("broken.rs in report");
    match &row.status {
        FileStatus::Updated {
            had_errors,
            error_count,
        } => {
            assert!(had_errors, "had_errors should be true");
            assert!(*error_count > 0, "error_count should be > 0");
        }
        other => panic!("expected Updated, got {other:?}"),
    }

    // Recovered symbols are still queryable.
    let outline = basemind::query::file_outline(&store, "broken.rs").unwrap();
    assert!(outline.had_errors);
    let names: Vec<&str> = outline.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"ok_one"),
        "well-formed sibling should still be extracted; got {names:?}"
    );
}

#[test]
fn scan_paths_only_touches_listed_files() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(root.join("a.rs"), b"pub fn a() {}\n").unwrap();
    fs::write(root.join("b.rs"), b"pub fn b() {}\n").unwrap();
    fs::write(root.join("c.rs"), b"pub fn c() {}\n").unwrap();

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();

    let hash_b_before = store.lookup("b.rs").unwrap().hash_hex.clone();
    let hash_c_before = store.lookup("c.rs").unwrap().hash_hex.clone();

    // Mutate a.rs only.
    fs::write(root.join("a.rs"), b"pub fn a_changed() {}\n").unwrap();

    let report = scan_paths(root, &mut store, &cfg, &[root.join("a.rs")]).unwrap();
    assert_eq!(report.stats.scanned, 1, "scan_paths visited only one file");
    assert_eq!(report.stats.updated, 1);

    // The unchanged files keep their original hashes.
    assert_eq!(store.lookup("b.rs").unwrap().hash_hex, hash_b_before);
    assert_eq!(store.lookup("c.rs").unwrap().hash_hex, hash_c_before);

    // The mutated file's symbol has changed.
    let hits = basemind::query::search_symbols(&store, "a_changed", None).unwrap();
    assert_eq!(hits.len(), 1);
}

// ─── Stage 2: query coverage gaps (TSX, arrow functions, Rust impl) ────────────

/// `const Foo = () => { … }` should surface as kind `function`, not `const`. The dedupe
/// pass in `extract/l1.rs` promotes the generic-`@symbol.const` match to function when the
/// more specific arrow-function pattern also fires.
#[test]
fn ts_arrow_function_const_is_function_kind() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(
        root.join("a.ts"),
        b"export const Greet = (name: string) => `hi ${name}`;\nexport const N: number = 1;\n",
    )
    .unwrap();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();

    let hits = basemind::query::search_symbols(&store, "Greet", None).unwrap();
    assert_eq!(hits.len(), 1, "arrow-fn const should produce one symbol");
    assert_eq!(
        hits[0].symbol.kind,
        SymbolKind::Function,
        "arrow-fn const should be kind=function"
    );

    let hits = basemind::query::search_symbols(&store, "N", None).unwrap();
    assert_eq!(hits.len(), 1, "non-function const stays as one symbol");
    assert_eq!(
        hits[0].symbol.kind,
        SymbolKind::Const,
        "regular const stays kind=const"
    );
}

#[test]
fn js_function_expression_const_is_function_kind() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(
        root.join("a.js"),
        b"const Greet = function(name) { return 'hi ' + name; };\n",
    )
    .unwrap();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();

    let hits = basemind::query::search_symbols(&store, "Greet", None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].symbol.kind, SymbolKind::Function);
}

#[test]
fn rust_impl_block_is_impl_kind() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(
        root.join("a.rs"),
        b"pub struct Foo;\nimpl Foo { pub fn bar(&self) {} }\n",
    )
    .unwrap();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();

    let impls = basemind::query::search_symbols(&store, "Foo", Some(SymbolKind::Impl)).unwrap();
    assert_eq!(impls.len(), 1, "expected an impl block for Foo");
    assert_eq!(impls[0].symbol.kind, SymbolKind::Impl);

    // The struct itself coexists, not replaced by the impl.
    let structs = basemind::query::search_symbols(&store, "Foo", Some(SymbolKind::Struct)).unwrap();
    assert_eq!(structs.len(), 1);
}

// ─── Stage 3: tree-sitter robustness ──────────────────────────────────────────

/// A binary-shaped file masquerading as TypeScript via its extension should be skipped
/// before the parser is invoked, not turned into an empty-symbols entry.
#[test]
fn binary_file_with_source_extension_is_skipped() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    // Synthetic content with a NUL in the first few bytes — looks_binary catches it.
    let mut payload = vec![0x89, b'P', b'N', b'G', 0x00, 0x01, 0x02, 0x03];
    payload.extend_from_slice(&[0u8; 64]);
    fs::write(root.join("not_really.ts"), &payload).unwrap();

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    let report = scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();

    assert_eq!(
        report.stats.skipped_binary, 1,
        "expected the .ts-named binary to be classified as binary"
    );
    assert!(
        store.lookup("not_really.ts").is_none(),
        "binary should not be indexed"
    );
}

/// `.tsx` files route to the dedicated tsx query (which mirrors typescript today but lives
/// in its own file so future JSX-specific captures don't disturb plain-TS files).
#[test]
fn tsx_file_uses_tsx_query() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(
        root.join("App.tsx"),
        b"export const App = () => (<div>hello</div>);\n",
    )
    .unwrap();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();

    let entry = store.lookup("App.tsx").expect("App.tsx indexed");
    assert_eq!(entry.language, "tsx");
    let hits = basemind::query::search_symbols(&store, "App", None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].symbol.kind, SymbolKind::Function);
}

#[test]
fn scan_paths_purges_removed_files() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(root.join("a.rs"), b"pub fn a() {}\n").unwrap();

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    assert!(store.lookup("a.rs").is_some());

    fs::remove_file(root.join("a.rs")).unwrap();
    let report = scan_paths(root, &mut store, &cfg, &[root.join("a.rs")]).unwrap();
    assert_eq!(report.stats.removed, 1);
    assert!(store.lookup("a.rs").is_none());
}

#[test]
fn ts_namespace_is_namespace_kind() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(
        root.join("ns.ts"),
        b"namespace Outer {\n  export const x: number = 1;\n}\n",
    )
    .unwrap();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();

    let hits = basemind::query::search_symbols(&store, "Outer", None).unwrap();
    assert_eq!(hits.len(), 1, "expected one Outer namespace hit");
    assert_eq!(
        hits[0].symbol.kind,
        SymbolKind::Namespace,
        "namespace Outer should be kind=namespace"
    );
}

#[test]
fn ts_getter_and_setter_kinds() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(
        root.join("c.ts"),
        b"class Box {\n  private _x: number = 0;\n  get x(): number { return this._x; }\n  set x(v: number) { this._x = v; }\n}\n",
    )
    .unwrap();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();

    let hits = basemind::query::search_symbols(&store, "x", None).unwrap();
    let getter = hits
        .iter()
        .find(|h| h.symbol.kind == SymbolKind::Getter)
        .expect("getter x should surface as kind=getter");
    let setter = hits
        .iter()
        .find(|h| h.symbol.kind == SymbolKind::Setter)
        .expect("setter x should surface as kind=setter");
    assert_eq!(getter.symbol.name, "x");
    assert_eq!(setter.symbol.name, "x");
}

#[test]
fn python_decorators_attach_to_symbol() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(
        root.join("d.py"),
        b"@dataclass\n@total_ordering\nclass Point:\n    x: int\n    y: int\n\n@property\ndef name(self):\n    return self._name\n",
    )
    .unwrap();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();

    let hits = basemind::query::search_symbols(&store, "Point", None).unwrap();
    let point = hits
        .iter()
        .find(|h| h.symbol.kind == SymbolKind::Class)
        .expect("Point class should be present");
    assert!(
        point.symbol.decorators.contains(&"@dataclass".to_string()),
        "Point should carry @dataclass; got {:?}",
        point.symbol.decorators
    );
    assert!(
        point
            .symbol
            .decorators
            .contains(&"@total_ordering".to_string()),
        "Point should carry @total_ordering; got {:?}",
        point.symbol.decorators
    );

    let hits = basemind::query::search_symbols(&store, "name", None).unwrap();
    let name = hits
        .iter()
        .find(|h| h.symbol.kind == SymbolKind::Function)
        .expect("name function should be present");
    assert!(
        name.symbol.decorators.contains(&"@property".to_string()),
        "name should carry @property; got {:?}",
        name.symbol.decorators
    );
}

// Skipped on macOS — APFS rejects non-UTF-8 filenames with EILSEQ at fs::write time. The
// Linux CI runner exercises the real filesystem-level non-UTF-8 path; the JSON / msgpack
// round-trip is covered cross-platform by the unit tests in `src/path.rs`.
#[cfg(target_os = "linux")]
#[test]
fn scanner_preserves_non_utf8_filename_bytes() {
    use std::os::unix::ffi::OsStrExt;

    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    // Build a filename containing an invalid UTF-8 lead byte (0xff). On Unix, paths are
    // raw bytes — std::fs::write happily creates this file.
    let raw_bytes: &[u8] = b"f\xffoo.rs";
    let bad_name = std::ffi::OsStr::from_bytes(raw_bytes);
    fs::write(root.join(bad_name), b"pub fn from_bad_path() {}\n").unwrap();

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    let report = scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();
    assert!(
        report.stats.updated >= 1,
        "scanner should index files with non-UTF-8 names; updated={}",
        report.stats.updated
    );
    // The path should round-trip through the on-disk index as raw bytes.
    let key = basemind::path::RelPath::from(raw_bytes);
    let entry = store
        .lookup(&key)
        .expect("non-UTF-8 path should be in index");
    assert_eq!(entry.language, "rust");
}

/// End-to-end check that kreuzberg's `whatlang`-backed language detector is
/// wired through `DocConfig::to_kreuzberg`. The fixture is a short French
/// paragraph; with `auto_detect = true` and the default 0.8 confidence floor,
/// `FileMapDoc.detected_languages` should carry the ISO 639-3 code `"fra"`.
/// (Kreuzberg's `ExtractionResult.detected_languages` doc-comment mislabels
/// the codes as ISO 639-1, but the wrapper normalises every variant to its
/// three-letter ISO 639-3 form before populating the field.)
#[cfg(feature = "documents")]
#[test]
fn scan_detects_french_in_markdown_fixture() {
    use std::fs;

    use basemind::config::DocLanguageConfig;
    use basemind::extract::doc::{DocConfig, extract_doc};

    let dir = tempfile::tempdir().expect("tempdir");
    let dst = dir.path().join("sample.md");
    let src = std::path::Path::new("tests/fixtures/french_doc/sample.md");
    fs::copy(src, &dst).expect("copy french fixture");

    let cfg = DocConfig {
        // Embeddings are expensive (model download) and unrelated to language
        // detection; skip them so the test stays offline-friendly.
        embed: false,
        embedding_preset: None,
        language: DocLanguageConfig {
            auto_detect: true,
            ..DocLanguageConfig::default()
        },
        ..DocConfig::default()
    };

    let doc = extract_doc(&dst, Some("text/markdown"), &cfg).expect("extract french doc");
    assert!(
        doc.detected_languages.iter().any(|l| l == "fra"),
        "expected ISO 639-3 'fra' in detected_languages; got {:?}",
        doc.detected_languages
    );
}

/// YAKE keyword extraction runs entirely in-process — no model download — so
/// the test runs unconditionally. We assert at least one keyword surfaces from
/// a topical paragraph; we don't pin the exact string because YAKE's ranking
/// can shift slightly across versions, but presence is a stable lower bound.
#[cfg(feature = "documents")]
#[test]
fn extract_doc_surfaces_keywords_when_enabled() {
    use std::fs;
    use std::path::Path;

    use basemind::config::{KeywordAlgorithm, KeywordsConfig};
    use basemind::extract::doc::{DocConfig, extract_doc};

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("article.txt");
    // Topical text so YAKE has obvious phrases to lock onto.
    let body = "Climate change is reshaping global agriculture. Farmers in tropical regions \
                report shifting rainfall patterns and crop yields are declining year over year. \
                Climate adaptation strategies include drought-resistant seed varieties and \
                precision irrigation. Climate scientists warn that without aggressive emissions \
                reductions, food security will deteriorate further across vulnerable regions.";
    fs::write(&path, body).expect("write fixture");

    let cfg = DocConfig {
        embed: false,
        embedding_preset: None,
        keywords: KeywordsConfig {
            enabled: true,
            algorithm: KeywordAlgorithm::Yake,
            max_keywords: 10,
            ..KeywordsConfig::default()
        },
        ..DocConfig::default()
    };
    let doc = extract_doc(Path::new(&path), Some("text/plain"), &cfg).expect("extract");
    assert!(
        !doc.keywords.is_empty(),
        "YAKE should surface at least one keyword for topical text; got empty list"
    );
    assert!(
        doc.keywords.iter().all(|k| k.algorithm == "yake"),
        "every keyword should be tagged with the algorithm used to produce it"
    );
}

/// End-to-end keywords + NER assertion. NER (gline-rs ONNX) downloads ~250 MB
/// of weights on first run, so the test is `#[ignore]`-gated. Pre-warm with:
/// `cargo test --features documents scan_extracts_keywords_and_entities -- --ignored`.
#[cfg(feature = "documents")]
#[test]
#[ignore = "downloads gline-rs ONNX weights (~250MB) on first run; pre-warm explicitly"]
fn scan_extracts_keywords_and_entities() {
    use std::fs;
    use std::path::Path;

    use basemind::config::{KeywordAlgorithm, KeywordsConfig, NerBackend, NerConfig};
    use basemind::extract::doc::{DocConfig, extract_doc};

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("press_release.txt");
    let body = "Microsoft Corporation announced a new partnership with the city of Paris \
                on Tuesday. Contact alice@example.com for media inquiries. The collaboration \
                will focus on artificial intelligence research and sustainable computing \
                infrastructure across Europe.";
    fs::write(&path, body).expect("write fixture");

    let cfg = DocConfig {
        embed: false,
        embedding_preset: None,
        keywords: KeywordsConfig {
            enabled: true,
            algorithm: KeywordAlgorithm::Yake,
            max_keywords: 10,
            ..KeywordsConfig::default()
        },
        ner: NerConfig {
            enabled: true,
            backend: NerBackend::Onnx,
            ..NerConfig::default()
        },
        ..DocConfig::default()
    };

    let doc = extract_doc(Path::new(&path), Some("text/plain"), &cfg).expect("extract");
    assert!(
        !doc.keywords.is_empty(),
        "keywords pipeline should produce at least one hit on the fixture"
    );
    assert!(
        !doc.entities.is_empty(),
        "NER pipeline should produce at least one entity on the fixture"
    );
    // We don't pin the exact label set — gline-rs models evolve — but at least
    // ONE entity should land in a structured category (i.e. not custom-empty).
    assert!(
        doc.entities.iter().any(|e| matches!(
            e.category.as_str(),
            "person" | "organization" | "location" | "email"
        )),
        "expected at least one standard-category entity; got {:?}",
        doc.entities.iter().map(|e| &e.category).collect::<Vec<_>>()
    );
}

#[test]
fn ts_multiline_generic_signature_is_collapsed() {
    let (dir, cfg) = fresh_repo();
    let root = dir.path();
    fs::write(
        root.join("g.ts"),
        b"function foo<\n  T extends Bar,\n  U extends Baz,\n>(x: T): U {\n  return x as unknown as U;\n}\n",
    )
    .unwrap();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    scan(
        root,
        &mut store,
        &cfg,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .unwrap();

    let hits = basemind::query::search_symbols(&store, "foo", None).unwrap();
    assert_eq!(hits.len(), 1);
    let sig = hits[0]
        .symbol
        .signature
        .as_deref()
        .expect("signature should be present");
    // Signature should be on one line, contain both generic params, and stop before the brace.
    assert!(
        sig.contains("T extends Bar") && sig.contains("U extends Baz"),
        "signature lost generic params: {sig}"
    );
    assert!(
        !sig.contains('{') && !sig.contains('\n'),
        "signature should be collapsed and stop at brace: {sig}"
    );
}
