//! `docs_search` metadata: read from the document map and applied to the hits, over a store
//! fixture written by hand so no embedding model is needed.

use std::sync::Arc;

use super::{DocMeta, apply_doc_metadata, read_doc_metadata};
use crate::extract::doc::{DocBlobs, DocEntity, DocKeyword, DocSummary, FileMapDoc};
use crate::mcp::types_documents::DocumentSearchHit;
use crate::store::{DocEntry, Store, VIEW_WORKING};

fn keyword(text: &str) -> DocKeyword {
    DocKeyword {
        text: text.to_string(),
        score: 0.1,
        algorithm: "yake".to_string(),
    }
}

fn entity(category: &str, text: &str) -> DocEntity {
    DocEntity {
        category: category.to_string(),
        text: text.to_string(),
        start: 0,
        end: text.len() as u32,
        confidence: None,
    }
}

/// Index `path` as a document carrying `keywords` and `entities`.
fn index_doc(store: &mut Store, path: &str, keywords: Vec<DocKeyword>, entities: Vec<DocEntity>) {
    let hash = crate::hashing::hash_bytes(path.as_bytes());
    let doc = FileMapDoc {
        schema_ver: crate::extract::SCHEMA_VER,
        mime_type: "text/markdown".to_string(),
        content: format!("content of {path}"),
        metadata: Vec::new(),
        detected_languages: Vec::new(),
        chunks: Vec::new(),
        embedding_model: String::new(),
        embedding_dim: 0,
        keywords,
        entities,
        summary: Some(DocSummary {
            text: format!("summary of {path}"),
            strategy: "extractive".to_string(),
            token_count: None,
        }),
        language_confidences: Vec::new(),
        config_digest: String::new(),
        linear_chunked: false,
        text_fallback: false,
    };
    store.write_doc(&hash, &doc).expect("write doc blob");
    store.upsert_doc(
        path,
        DocEntry {
            hash_hex: crate::hashing::hex(&hash),
            embedding_preset: String::new(),
            size_bytes: 1,
            mtime: 0,
            embedded: false,
            embed_attempted: false,
            config_digest: String::new(),
        },
    );
}

fn hit(path: &str, chunk_idx: u32) -> DocumentSearchHit {
    DocumentSearchHit {
        path: path.to_string(),
        chunk_idx,
        text: format!("chunk {chunk_idx} of {path}"),
        mime_type: "text/markdown".to_string(),
        byte_start: 0,
        byte_end: 1,
        distance: 0.0,
        rerank_score: None,
        keywords: Vec::new(),
        entities: Vec::new(),
        summary: None,
    }
}

fn fixture() -> (tempfile::TempDir, Store) {
    crate::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(dir.path(), VIEW_WORKING).expect("open store");
    index_doc(
        &mut store,
        "docs/harbour.md",
        vec![keyword("Harbour pilots"), keyword("tides")],
        vec![entity("person", "Ada")],
    );
    index_doc(&mut store, "docs/airship.md", vec![keyword("zeppelin")], Vec::new());
    (dir, store)
}

fn metadata(store: &Store, paths: &[&str]) -> ahash::AHashMap<String, Arc<DocMeta>> {
    read_doc_metadata(store, paths).into_iter().collect()
}

fn paths(hits: &[DocumentSearchHit]) -> Vec<(&str, u32)> {
    hits.iter().map(|hit| (hit.path.as_str(), hit.chunk_idx)).collect()
}

#[test]
fn metadata_is_read_through_the_document_map_and_a_path_it_does_not_hold_has_none() {
    let (_dir, store) = fixture();
    let meta = metadata(&store, &["docs/harbour.md", "docs/gone.md", "docs/airship.md"]);
    assert_eq!(meta.len(), 2, "a path deleted since the scan is not an error");
    let harbour = &meta["docs/harbour.md"];
    assert_eq!(harbour.0.len(), 2);
    assert_eq!(harbour.1[0].category, "person");
    assert_eq!(harbour.2.as_ref().expect("summary").text, "summary of docs/harbour.md");
}

#[test]
fn every_chunk_of_a_document_carries_its_metadata() {
    let (_dir, store) = fixture();
    let meta = metadata(&store, &["docs/harbour.md", "docs/airship.md"]);
    let mut hits = vec![
        hit("docs/harbour.md", 0),
        hit("docs/airship.md", 0),
        hit("docs/harbour.md", 1),
    ];
    apply_doc_metadata(&mut hits, &meta, None, None);
    assert_eq!(hits.len(), 3, "no filter drops nothing");
    assert_eq!(hits[0].keywords.len(), 2);
    assert_eq!(
        hits[2].keywords, hits[0].keywords,
        "the second chunk of the same document"
    );
    assert_eq!(hits[1].keywords[0].text, "zeppelin");
    assert!(hits.iter().all(|hit| hit.summary.is_some()));
}

#[test]
fn the_keyword_and_entity_filters_keep_matching_documents_in_rank_order() {
    let (_dir, store) = fixture();
    let meta = metadata(&store, &["docs/harbour.md", "docs/airship.md"]);
    let ranked = || {
        vec![
            hit("docs/airship.md", 0),
            hit("docs/harbour.md", 0),
            hit("docs/harbour.md", 1),
        ]
    };

    let mut by_keyword = ranked();
    apply_doc_metadata(&mut by_keyword, &meta, None, Some("PILOTS"));
    assert_eq!(
        paths(&by_keyword),
        [("docs/harbour.md", 0), ("docs/harbour.md", 1)],
        "case-insensitive substring of a keyword"
    );

    let mut by_entity = ranked();
    apply_doc_metadata(&mut by_entity, &meta, Some("Person"), None);
    assert_eq!(paths(&by_entity), [("docs/harbour.md", 0), ("docs/harbour.md", 1)]);

    let mut both = ranked();
    apply_doc_metadata(&mut both, &meta, Some("person"), Some("zeppelin"));
    assert!(both.is_empty(), "no document has both");

    let mut unknown = vec![hit("docs/gone.md", 0), hit("docs/airship.md", 0)];
    apply_doc_metadata(&mut unknown, &meta, None, Some("zeppelin"));
    assert_eq!(
        paths(&unknown),
        [("docs/airship.md", 0)],
        "a document with no metadata fails a filter that is set"
    );
    let mut unfiltered = vec![hit("docs/gone.md", 0)];
    apply_doc_metadata(&mut unfiltered, &meta, None, None);
    assert_eq!(unfiltered.len(), 1, "and passes when none is");
}
