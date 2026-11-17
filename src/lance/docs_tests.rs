use super::*;
use crate::lance::docs_schema::DocChunkRow;

const DIM: u16 = 4;

fn chunk(ord: u32, text: &str, vector: Option<[f32; 4]>, cats: &[&str]) -> DocChunkRow {
    DocChunkRow {
        chunk_ord: ord,
        chunk_kind: "unknown".to_string(),
        page_start: Some(ord + 1),
        page_end: Some(ord + 1),
        section: Some(format!(
            "Guide{}Part {ord}",
            crate::lance::docs_schema::SECTION_SEPARATOR
        )),
        line_start: None,
        line_end: None,
        byte_start: ord * 10,
        byte_end: ord * 10 + 9,
        entity_cats: cats.iter().map(|c| c.to_string()).collect(),
        text: text.to_string(),
        embedding: vector.map(|v| v.to_vec()),
    }
}

fn doc(key: &str, mime: &str, keywords: &[&str], chunks: Vec<DocChunkRow>) -> DocRows {
    DocRows {
        doc_key: key.to_string(),
        mime_type: mime.to_string(),
        mime_group: mime_group(mime).to_string(),
        title: Some(format!("{key} title")),
        keywords: keywords.iter().map(|k| k.to_string()).collect(),
        chunks,
    }
}

fn store() -> (tempfile::TempDir, DocStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = DocStore::open(&dir.path().join("docs"), DIM, "test-model").unwrap();
    (dir, store)
}

fn seeded() -> (tempfile::TempDir, DocStore) {
    let (dir, store) = store();
    // The nearest rows to [1,0,0,0] are all markdown; the one PDF row is the farthest.
    let mut docs: Vec<DocRows> = (0..30)
        .map(|i| {
            let x = 1.0 - i as f32 * 0.01;
            doc(
                &format!("md{i:02}"),
                "text/markdown",
                &[],
                vec![chunk(
                    0,
                    &format!("harbour pilots note {i}"),
                    Some([x, 0.1, 0.0, 0.0]),
                    &[],
                )],
            )
        })
        .collect();
    docs.push(doc(
        "pdf",
        "application/pdf",
        &["flood barrier"],
        vec![
            chunk(
                0,
                "the flood barrier closes at high tide",
                Some([0.0, 0.0, 1.0, 0.0]),
                &["location"],
            ),
            chunk(1, "the barrier is inspected monthly", None, &[]),
        ],
    ));
    store.write(&docs).unwrap();
    store.maintain_indexes().unwrap();
    (dir, store)
}

#[test]
fn a_selective_filter_is_applied_before_the_cut() {
    let (_dir, store) = seeded();
    let predicate = DocPredicate {
        mime_type: Some("application/pdf".to_string()),
        ..DocPredicate::default()
    };
    // Five candidates: an after-the-cut filter would see only markdown and return nothing.
    let hits = store.nearest(&[1.0, 0.0, 0.0, 0.0], &predicate, 5).unwrap();
    assert_eq!(hits.len(), 1, "the null-vector chunk is not a vector hit");
    assert_eq!((hits[0].doc_key.as_str(), hits[0].chunk_ord), ("pdf", 0));
    assert_eq!(hits[0].page_start, Some(1));
    let by_kind = DocPredicate {
        mime_group: Some("pdf".to_string()),
        ..DocPredicate::default()
    };
    assert_eq!(store.nearest(&[1.0, 0.0, 0.0, 0.0], &by_kind, 5).unwrap().len(), 1);
}

#[test]
fn the_lexical_lane_finds_chunks_without_vectors_under_the_same_filter() {
    let (_dir, store) = seeded();
    assert!(store.capabilities().unwrap().lexical);
    let hits = store.lexical("inspected", &DocPredicate::default(), 10, false).unwrap();
    assert_eq!(
        hits.iter()
            .map(|h| (h.doc_key.as_str(), h.chunk_ord))
            .collect::<Vec<_>>(),
        vec![("pdf", 1)]
    );
    // Stemming: "barriers" matches "barrier".
    assert!(
        !store
            .lexical("barriers", &DocPredicate::default(), 10, false)
            .unwrap()
            .is_empty()
    );
    // Fuzzy: one edit.
    assert!(
        store
            .lexical("inspectd", &DocPredicate::default(), 10, false)
            .unwrap()
            .is_empty()
    );
    assert!(
        !store
            .lexical("inspectd", &DocPredicate::default(), 10, true)
            .unwrap()
            .is_empty()
    );
    let md_only = DocPredicate {
        mime_group: Some("markdown".to_string()),
        ..DocPredicate::default()
    };
    assert!(store.lexical("inspected", &md_only, 10, false).unwrap().is_empty());
}

#[test]
fn entities_filter_chunks_and_keywords_select_documents() {
    let (_dir, store) = seeded();
    let entity = DocPredicate {
        entity: Some("location".to_string()),
        ..DocPredicate::default()
    };
    let hits = store.lexical("barrier", &entity, 10, false).unwrap();
    assert_eq!(hits.iter().map(|h| h.chunk_ord).collect::<Vec<_>>(), vec![0]);
    assert_eq!(
        store.keys_with_keywords(&["flood barrier".to_string()]).unwrap(),
        vec!["pdf".to_string()]
    );
    assert!(
        store.keys_with_keywords(&["flood".to_string()]).unwrap().is_empty(),
        "whole keywords only"
    );
}

#[test]
fn live_keys_in_the_predicate_hide_every_other_document() {
    let (_dir, store) = seeded();
    let predicate = DocPredicate {
        keys: Some(vec!["md29".to_string()]),
        ..DocPredicate::default()
    };
    let hits = store.nearest(&[1.0, 0.0, 0.0, 0.0], &predicate, 5).unwrap();
    assert_eq!(
        hits.iter().map(|h| h.doc_key.as_str()).collect::<Vec<_>>(),
        vec!["md29"]
    );
    let none = DocPredicate {
        keys: Some(Vec::new()),
        ..DocPredicate::default()
    };
    assert!(store.nearest(&[1.0, 0.0, 0.0, 0.0], &none, 5).unwrap().is_empty());
}

#[test]
fn a_write_is_present_once_and_a_rewrite_with_vectors_replaces_it() {
    let (_dir, store) = store();
    let bare = doc(
        "k",
        "text/markdown",
        &[],
        vec![chunk(0, "a", None, &[]), chunk(1, "b", None, &[])],
    );
    store.write(std::slice::from_ref(&bare)).unwrap();
    assert_eq!(
        store.present(&["k".to_string(), "x".to_string()]).unwrap().get("k"),
        Some(&false)
    );
    let embedded = doc(
        "k",
        "text/markdown",
        &[],
        vec![chunk(0, "a", Some([1.0; 4]), &[]), chunk(1, "b", Some([1.0; 4]), &[])],
    );
    store.write(&[embedded]).unwrap();
    assert_eq!(store.present(&["k".to_string()]).unwrap().get("k"), Some(&true));
    assert_eq!(store.capabilities().unwrap().rows, 2, "replaced, not appended");
    assert_eq!(store.capabilities().unwrap().vectors, 2);
}

#[test]
fn neighbours_and_document_rows_are_read_by_key() {
    let (_dir, store) = seeded();
    let chunks = store
        .chunks(&[("pdf".to_string(), vec![1]), ("md03".to_string(), vec![0, 5])])
        .unwrap();
    let mut got: Vec<(String, u32)> = chunks.into_iter().map(|c| (c.doc_key, c.chunk_ord)).collect();
    got.sort();
    assert_eq!(got, vec![("md03".to_string(), 0), ("pdf".to_string(), 1)]);
    let meta = store.documents(&["pdf".to_string()]).unwrap();
    assert_eq!(
        (meta[0].title.as_deref(), meta[0].n_chunks, meta[0].embedded),
        (Some("pdf title"), 2, false)
    );
}

#[test]
fn dead_documents_are_deleted_and_live_ones_kept() {
    let (_dir, store) = seeded();
    let live: AHashSet<String> = ["pdf".to_string(), "md00".to_string()].into_iter().collect();
    assert_eq!(store.delete_dead(&live).unwrap(), 29);
    let mut keys = store.keys().unwrap();
    keys.sort();
    assert_eq!(keys, vec!["md00".to_string(), "pdf".to_string()]);
    assert_eq!(store.capabilities().unwrap().rows, 3);
}

#[test]
fn keys_name_content_and_settings_and_store_dirs_name_the_model() {
    assert_eq!(doc_key("aa", "d1"), doc_key("aa", "d1"));
    assert_ne!(doc_key("aa", "d1"), doc_key("aa", "d2"));
    assert_ne!(doc_key("a", "ad1"), doc_key("aa", "d1"), "parts are length-prefixed");
    assert_eq!(doc_key("aa", "d1").len(), 32);
    let parent = Path::new("/ws");
    assert_ne!(store_dir(parent, "a/b", 768), store_dir(parent, "a-b", 768));
    assert!(store_dir(parent, "balanced", 768).starts_with("/ws"));
    assert_eq!(mime_group("application/pdf"), "pdf");
    assert_eq!(mime_group("text/markdown"), "markdown");
    assert_eq!(
        mime_group("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        "office"
    );
    assert_eq!(mime_group("message/rfc822"), "email");
    assert_eq!(mime_group("application/x-ipynb+json"), "notebook");
    assert_eq!(mime_group("text/html; charset=utf-8"), "html");
}
