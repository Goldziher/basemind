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
        ..DocRows::default()
    }
}

fn store() -> (tempfile::TempDir, DocStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = DocStore::open(&dir.path().join("docs"), DIM, "test-model").unwrap();
    (dir, store)
}

fn seeded() -> (tempfile::TempDir, DocStore) {
    let (dir, store) = store();
    seed(&store);
    (dir, store)
}

/// Write the shared fixture's documents to `store` and build their indexes.
fn seed(store: &DocStore) {
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

/// A rank whose exact lexical hits are few reruns fuzzy, which matches unstemmed terms and can find
/// fewer: the exact hits must survive it.
#[test]
fn a_fuzzy_rerun_that_finds_less_keeps_the_exact_hits() {
    let (_dir, store) = seeded();
    assert!(
        store
            .lexical("inspected", &DocPredicate::default(), 10, true)
            .unwrap()
            .is_empty()
    );
    let request = crate::lance::docs_rank::DocsRequest {
        query: "inspected".to_string(),
        ..crate::lance::docs_rank::DocsRequest::default()
    };
    let filters = crate::lance::docs_rank::Filters::parse(&request, None).unwrap();
    let mut live = crate::lance::docs_rank::LiveDocs::default();
    live.by_key.insert("pdf".to_string(), vec!["manual.pdf".to_string()]);
    let inputs = crate::lance::docs_rank::RankInputs {
        store: Some(&store),
        live,
        owed: 0,
        embed: Err("off".to_string()),
        rerank: None,
    };
    let ranking = crate::lance::docs_rank::rank(&request.query, &filters, inputs).unwrap();
    assert!(!ranking.lexical_fuzzy);
    assert_eq!(
        ranking.hits.iter().map(|h| (h.doc.as_str(), h.ord)).collect::<Vec<_>>(),
        vec![("pdf", 1)]
    );
}

/// A search cancelled before it starts runs no lane and says it was cancelled.
#[test]
fn a_cancelled_rank_runs_no_lane() {
    let (_dir, store) = seeded();
    let request = crate::lance::docs_rank::DocsRequest {
        query: "barrier".to_string(),
        ..crate::lance::docs_rank::DocsRequest::default()
    };
    let filters = crate::lance::docs_rank::Filters::parse(&request, None).unwrap();
    let mut live = crate::lance::docs_rank::LiveDocs::default();
    live.by_key.insert("pdf".to_string(), vec!["manual.pdf".to_string()]);
    let inputs = crate::lance::docs_rank::RankInputs {
        store: Some(&store),
        live,
        owed: 0,
        embed: Err("off".to_string()),
        rerank: None,
    };
    let outcome = crate::lance::docs_rank::rank_until(&request.query, &filters, inputs, &|| true);
    assert_eq!(
        outcome.map(|ranking| ranking.hits.len()),
        Err(crate::lance::docs_rank::DocsSearchError::Cancelled)
    );
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

#[test]
fn live_keys_and_metadata_filters_apply_together() {
    let (_dir, store) = seeded();
    // Both documents are live; only one of them is a PDF.
    let predicate = DocPredicate {
        mime_type: Some("application/pdf".to_string()),
        keys: Some(vec!["md00".to_string(), "pdf".to_string()]),
        ..DocPredicate::default()
    };
    let vector = store.nearest(&[1.0, 0.0, 0.0, 0.0], &predicate, 5).unwrap();
    assert_eq!(
        vector.iter().map(|h| h.doc_key.as_str()).collect::<Vec<_>>(),
        vec!["pdf"]
    );
    let lexical = store.lexical("harbour barrier", &predicate, 10, false).unwrap();
    assert!(lexical.iter().all(|h| h.doc_key == "pdf"), "{lexical:?}");
    assert!(!lexical.is_empty());
}

/// A search opens the store without the group lock: a store of another model or version is refused
/// and left as it is, never wiped from under the scan that writes it.
#[test]
fn a_search_side_open_of_a_mismatched_store_refuses_and_wipes_nothing() {
    let (dir, store) = store();
    store
        .write(&[doc("kept", "text/markdown", &[], vec![chunk(0, "kept", None, &[])])])
        .unwrap();
    drop(store);
    let path = dir.path().join("docs");
    let meta = std::fs::read(path.join(crate::lance::META_FILE)).unwrap();

    let refused = DocStore::shared_existing(&path, DIM, "another-model");
    assert!(refused.is_err(), "another model is refused");
    assert_eq!(std::fs::read(path.join(crate::lance::META_FILE)).unwrap(), meta);
    let reopened = DocStore::shared_existing(&path, DIM, "test-model")
        .unwrap()
        .expect("store exists");
    assert!(reopened.present(&["kept".to_string()]).unwrap().contains_key("kept"));
}

/// A writer's open of the store a search opened first keeps the search's handle. Lance caches an
/// index with the IO loop of the store runtime that loaded it; a second store on the directory read
/// that cached index (the posting lists of terms the first search never asked for) through a loop
/// nobody drove once the search's store was dropped, and the search hung for ever.
#[test]
fn a_writer_open_after_a_search_open_keeps_its_store_and_later_searches_answer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("docs");
    // Seeded through a session of its own, so nothing this process caches comes from its runtime.
    let seeding = DocStore {
        lance: LanceStore::open_with(
            &path,
            DIM,
            "test-model",
            crate::lance::OPEN_LOCK_WAIT,
            &tables(DIM),
            crate::lance::session::bounded(1 << 20, 1 << 20),
        )
        .unwrap(),
        gate: sweep_gate(&path),
        created: true,
    };
    seed(&seeding);
    drop(seeding);

    let searched = DocStore::shared_existing(&path, DIM, "test-model")
        .unwrap()
        .expect("store exists");
    assert_eq!(
        searched
            .lexical("harbour", &DocPredicate::default(), 10, false)
            .unwrap()
            .len(),
        10
    );
    let written = DocStore::shared(&path, DIM, "test-model").unwrap();
    assert!(
        std::sync::Arc::ptr_eq(&searched.lance.inner, &written.lance.inner),
        "the writer opened a second store beside the search's"
    );
    drop(searched);

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let hits = written
            .lexical("barrier", &DocPredicate::default(), 10, false)
            .map(|hits| {
                let mut found: Vec<(String, u32)> = hits.into_iter().map(|h| (h.doc_key, h.chunk_ord)).collect();
                found.sort_unstable();
                found
            })
            .map_err(|error| format!("{error:#}"));
        tx.send(hits).ok();
    });
    let hits = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a search after the writer's open hung");
    assert_eq!(hits, Ok(vec![("pdf".to_string(), 0), ("pdf".to_string(), 1)]));
}

/// A handle a search opened is not adopted once its directory was rebuilt for another dimension:
/// the writer opens the store again.
#[test]
fn a_writer_open_reopens_a_store_rebuilt_since_the_search_opened_it() {
    let (dir, store) = store();
    seed(&store);
    drop(store);
    let path = dir.path().join("docs");
    let searched = DocStore::shared_existing(&path, DIM, "test-model")
        .unwrap()
        .expect("store exists");
    let meta = path.join(crate::lance::META_FILE);
    let before = std::fs::read_to_string(&meta).unwrap();
    let rebuilt = before.replace("\"dim\": 4", "\"dim\": 8");
    assert_ne!(rebuilt, before, "the fixture rewrites the recorded dimension");
    std::fs::write(&meta, rebuilt).unwrap();
    assert!(!searched.lance.adopt_tables(&tables(DIM)).unwrap());
}
