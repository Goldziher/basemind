//! The background embedding pass a one-shot scan hands its vectors to (#454): the scan writes the
//! documents' text without calling the embedder and records what it left owed, and the worker
//! embeds only what is still owed, so one that was killed is resumed where it stopped. The
//! embedder is a stand-in, so no model is loaded.

#![cfg(feature = "documents")]

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use basemind::cli::embed_worker::{Pending, run_local};
use basemind::config::ConfigV1;
use basemind::embeddings::test_document_embedder;
use basemind::scanner::{EmbedMode, ScanSource, scan, scan_paths};
use basemind::store::{Store, VIEW_WORKING};

/// The stand-in embedder is process-wide: the tests that set it run one at a time.
static EMBEDDER: Mutex<()> = Mutex::new(());

/// Calls that reached the stand-in.
static CALLS: AtomicUsize = AtomicUsize::new(0);

/// Every text the stand-in embedded.
static EMBEDDED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn refuse(_: &[&str], _: u16) -> Result<Vec<Vec<f32>>, String> {
    CALLS.fetch_add(1, Ordering::SeqCst);
    Err("the one-shot scan must not embed".to_string())
}

fn record(texts: &[&str], dim: u16) -> Result<Vec<Vec<f32>>, String> {
    CALLS.fetch_add(1, Ordering::SeqCst);
    EMBEDDED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .extend(texts.iter().map(|text| (*text).to_string()));
    Ok(texts.iter().map(|_| vec![0.5; usize::from(dim)]).collect())
}

fn embedded_texts() -> Vec<String> {
    std::mem::take(&mut *EMBEDDED.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
}

fn workspace() -> tempfile::TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("harbour.md"),
        "# Harbour\n\nHow the harbour pilots bring the ferries in at night.\n",
    )
    .expect("harbour.md");
    std::fs::write(
        dir.path().join("orchard.md"),
        "# Orchard\n\nWhen the orchard apples are picked and pressed into cider.\n",
    )
    .expect("orchard.md");
    dir
}

fn one_shot_scan(root: &Path, config: &ConfigV1) -> Pending {
    let mut store = Store::open(root, VIEW_WORKING).expect("open store");
    scan(
        root,
        &mut store,
        config,
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
        basemind::retrieval_wiring::backend(),
    )
    .expect("scan");
    Pending::of(root, &store, config)
}

fn embedded(root: &Path, rel: &str) -> bool {
    let store = Store::open(root, VIEW_WORKING).expect("open store");
    store.lookup_doc(rel).is_some_and(|entry| entry.embedded)
}

#[test]
fn the_one_shot_scan_indexes_documents_without_the_embedder_and_counts_them_pending() {
    let _serial = EMBEDDER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = workspace();
    let config = ConfigV1::with_defaults();
    test_document_embedder::set(Some(refuse));
    CALLS.store(0, Ordering::SeqCst);
    let pending = one_shot_scan(dir.path(), &config);
    let calls = CALLS.load(Ordering::SeqCst);
    test_document_embedder::set(None);

    assert_eq!(calls, 0, "the scan called the embedder");
    assert_eq!(
        pending,
        Pending {
            documents: 2,
            code: false,
            policy: false,
        }
    );
    let store = Store::open(dir.path(), VIEW_WORKING).expect("open store");
    for rel in ["harbour.md", "orchard.md"] {
        let entry = store.lookup_doc(rel).expect("tracked by the scan");
        assert!(!entry.embedded && !entry.embed_attempted, "{rel}: {entry:?}");
    }
}

#[test]
fn the_worker_embeds_only_what_is_still_owed_and_settles_the_record() {
    let _serial = EMBEDDER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = workspace();
    let root = dir.path();
    let config = ConfigV1::with_defaults();
    test_document_embedder::set(Some(refuse));
    assert_eq!(one_shot_scan(root, &config).documents, 2);

    // A worker that was killed after its first batch: one document has its vectors.
    test_document_embedder::set(Some(record));
    {
        let mut store = Store::open(root, VIEW_WORKING).expect("open store");
        scan_paths(
            root,
            &mut store,
            &config,
            &[root.join("harbour.md")],
            EmbedMode::Inline,
            basemind::retrieval_wiring::backend(),
        )
        .expect("the first batch");
    }
    assert!(embedded(root, "harbour.md") && !embedded(root, "orchard.md"));
    embedded_texts();

    let report = run_local(root, &config, basemind::retrieval_wiring::backend()).expect("the resumed worker");
    let texts = embedded_texts();
    test_document_embedder::set(None);

    assert_eq!(report.documents_settled, 1, "{report:?}");
    assert_eq!(report.documents_owed, 0, "{report:?}");
    assert!(!report.stopped && !report.daemon, "{report:?}");
    assert!(
        report.whole_pass,
        "the first pass with vectors applies the embed policy: {report:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("orchard apples")),
        "the owed document was embedded: {texts:?}"
    );
    assert!(
        !texts.iter().any(|text| text.contains("harbour pilots")),
        "the document embedded before the kill was embedded again: {texts:?}"
    );
    assert!(embedded(root, "orchard.md"));
    let cache_dir = basemind::store::workspace_cache_dir(root);
    assert_eq!(
        basemind::embed_progress::read(&cache_dir),
        None,
        "the record is settled"
    );
}
