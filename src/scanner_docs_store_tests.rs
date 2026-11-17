use super::*;
use crate::extract::doc::{DocChunk, DocEntity, DocKeyword};
use crate::store::DocEntry;

fn blob(text: &str, vectors: bool) -> FileMapDoc {
    FileMapDoc {
        config_digest: "digest".to_string(),
        linear_chunked: false,
        text_fallback: false,
        embed_failed: false,
        schema_ver: crate::extract::SCHEMA_VER,
        mime_type: "text/markdown".to_string(),
        content: format!("{text} one\n{text} two\n"),
        metadata: vec![("title".to_string(), format!("{text} title"))],
        detected_languages: Vec::new(),
        chunks: vec![
            DocChunk {
                byte_start: 0,
                byte_end: 8,
                text: format!("{text} one"),
                embedding: if vectors { vec![0.5; 768] } else { Vec::new() },
                section: vec!["Top".to_string(), "Sub".to_string()],
                line_start: Some(1),
                line_end: Some(1),
                ..DocChunk::default()
            },
            DocChunk {
                byte_start: 8,
                byte_end: 16,
                text: format!("{text} two"),
                embedding: if vectors { vec![0.5; 768] } else { Vec::new() },
                ..DocChunk::default()
            },
        ],
        embedding_model: if vectors {
            crate::embeddings::model_key("balanced").into_owned()
        } else {
            String::new()
        },
        embedding_dim: if vectors { 768 } else { 0 },
        keywords: vec![DocKeyword {
            text: "Flood Barrier".to_string(),
            score: 0.1,
            algorithm: "yake".to_string(),
        }],
        entities: vec![DocEntity {
            category: "LOCATION".to_string(),
            text: "x".to_string(),
            start: 9,
            end: 10,
            confidence: None,
        }],
        summary: None,
        language_confidences: Vec::new(),
    }
}

/// Write `doc`'s blob and track it at each of `paths`; returns its content hash. Blobs are global to
/// the test process, so tests that run at once must not write one content with and without
/// vectors: each test uses its own texts unless it reads the same blob only.
fn track(store: &mut Store, doc: &FileMapDoc, paths: &[&str]) -> String {
    let hash = crate::hashing::hash_bytes(doc.content.as_bytes());
    store.write_doc(&hash, doc).unwrap();
    let hex = crate::hashing::hex(&hash);
    for path in paths {
        store.upsert_doc(
            *path,
            DocEntry {
                config_digest: "digest".to_string(),
                hash_hex: hex.clone(),
                embedding_preset: "balanced".to_string(),
                size_bytes: 1,
                mtime: 0,
                embedded: doc.embedding_dim > 0,
                embed_attempted: true,
            },
        );
    }
    hex
}

fn setup() -> (tempfile::TempDir, Store, Config, Filters) {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), crate::store::VIEW_WORKING).unwrap();
    let mut config = crate::config::default_for_root(std::path::Path::new("."));
    config.documents.embed = true;
    let filters = Filters::build(&config, Vec::new()).unwrap();
    (tmp, store, config, filters)
}

fn doc_store(store: &Store, config: &Config) -> DocStore {
    open_doc_store(store, config, false).unwrap().expect("store exists")
}

#[test]
fn rows_carry_citations_entities_keywords_and_title() {
    let rows = build_doc_rows(blob("alpha", true), "k".to_string(), true, 10);
    assert_eq!(rows.mime_group, "markdown");
    assert_eq!(rows.title.as_deref(), Some("alpha title"));
    assert_eq!(rows.keywords, vec!["flood barrier".to_string()]);
    assert_eq!(rows.chunks[0].section.as_deref(), Some("Top > Sub"));
    assert_eq!(rows.chunks[0].line_start, Some(1));
    assert!(rows.chunks[0].entity_cats.is_empty());
    assert_eq!(rows.chunks[1].entity_cats, vec!["location".to_string()]);
    assert!(rows.embedded());
    assert!(!build_doc_rows(blob("alpha", true), "k".to_string(), false, 10).embedded());
    assert!(
        build_doc_rows(blob("alpha", true), "k".to_string(), true, 1)
            .chunks
            .is_empty()
    );
}

#[test]
fn a_scan_writes_absent_documents_once_per_content_and_settles_their_paths() {
    let (_tmp, mut store, config, filters) = setup();
    track(&mut store, &blob("alpha", true), &["a.md", "copy/a.md"]);
    track(&mut store, &blob("beta", false), &["b.md"]);
    store.pending_vectors.owe_doc("a.md");
    assert_eq!(
        sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX),
        2,
        "two contents, three paths"
    );
    assert!(store.pending_vectors.docs().is_empty());
    let caps = doc_store(&store, &config).capabilities().unwrap();
    assert_eq!((caps.rows, caps.vectors), (4, 2));
    assert!(caps.lexical, "the scan builds the full-text index");
    assert_eq!(
        sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX),
        0,
        "nothing absent, nothing written"
    );
}

#[test]
fn a_wiped_store_is_rebuilt_from_the_blobs() {
    let (_tmp, mut store, config, filters) = setup();
    track(&mut store, &blob("wiped", true), &["a.md"]);
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);
    let dir = doc_store(&store, &config).dir().to_path_buf();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(
        sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX),
        1
    );
    assert_eq!(doc_store(&store, &config).capabilities().unwrap().vectors, 2);
}

#[test]
fn rows_without_vectors_are_replaced_once_the_document_is_embedded() {
    let (_tmp, mut store, config, filters) = setup();
    let bare = blob("bare", false);
    let hash = crate::hashing::hash_bytes(bare.content.as_bytes());
    track(&mut store, &bare, &["a.md"]);
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);
    assert_eq!(doc_store(&store, &config).capabilities().unwrap().vectors, 0);
    // The same content, now embedded: same key, the rows gain their vectors.
    track(&mut store, &blob("bare", true), &["a.md"]);
    assert_eq!(crate::hashing::hash_bytes(blob("bare", true).content.as_bytes()), hash);
    assert_eq!(
        sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX),
        1
    );
    assert_eq!(doc_store(&store, &config).capabilities().unwrap().vectors, 2);
}

#[test]
fn rows_no_document_names_are_deleted_and_removal_never_creates_a_store() {
    let (_tmp, mut store, config, filters) = setup();
    remove_dead_document_rows(&store, &config, &filters);
    assert!(open_doc_store(&store, &config, false).unwrap().is_none());
    track(&mut store, &blob("alpha", true), &["a.md"]);
    track(&mut store, &blob("beta", true), &["b.md"]);
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);
    store.remove_doc("b.md");
    remove_dead_document_rows(&store, &config, &filters);
    assert_eq!(doc_store(&store, &config).keys().unwrap().len(), 1);
}

#[test]
fn excluded_documents_lose_their_vectors_and_stay_searchable() {
    let (_tmp, mut store, mut config, _) = setup();
    track(&mut store, &blob("alpha", true), &["docs/a.md"]);
    track(&mut store, &blob("beta", true), &["notes/b.md"]);
    let filters = Filters::build(&config, Vec::new()).unwrap();
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);
    config.documents.embed_exclude = vec!["docs/**".to_string()];
    let filters = Filters::build(&config, Vec::new()).unwrap();
    assert!(strip_unwanted_vectors(&store, &config, &filters));
    let caps = doc_store(&store, &config).capabilities().unwrap();
    assert_eq!((caps.rows, caps.vectors), (4, 2));
    config.documents.embed = false;
    let filters = Filters::build(&config, Vec::new()).unwrap();
    assert!(strip_unwanted_vectors(&store, &config, &filters));
    assert_eq!(doc_store(&store, &config).capabilities().unwrap().vectors, 0);
}

/// Review M2: a document whose new version has no rows (zero chunks, over the chunk cap, excluded)
/// must not keep serving the old version's rows. Rows are keyed by content, so the old version's key
/// is dead once the path names the new content.
#[test]
fn a_new_version_without_rows_drops_the_old_versions_rows() {
    let (_tmp, mut store, config, filters) = setup();
    track(&mut store, &blob("version one", true), &["guide.md"]);
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);
    assert_eq!(doc_store(&store, &config).capabilities().unwrap().rows, 2);

    let mut empty = blob("version two", false);
    empty.chunks.clear();
    track(&mut store, &empty, &["guide.md"]);
    sync_document_rows(&mut store, &config, &filters, &AHashSet::new(), usize::MAX);
    remove_dead_document_rows(&store, &config, &filters);

    assert_eq!(
        doc_store(&store, &config).capabilities().unwrap().rows,
        0,
        "no row of the old version is served"
    );
}
