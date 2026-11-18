//! A document whose embedding failed is indexed without vectors and owes them: while the embedder
//! stays down it is not re-extracted on every scan, and once the embedder works the next scan
//! embeds it without the document changing. The embedder is a stand-in, so no model is loaded.

#![cfg(feature = "documents")]

use std::fs;
use std::path::Path;

use basemind::config::ConfigV1;
use basemind::embeddings::test_document_embedder;
use basemind::scanner::{CollectObserver, EmbedMode, FileStatus, ScanCancel, ScanSource, scan_with_observer};
use basemind::store::{Store, VIEW_WORKING};

fn down(_: &[&str], _: u16) -> Result<Vec<Vec<f32>>, String> {
    Err("the model could not be downloaded".to_string())
}

fn up(texts: &[&str], dim: u16) -> Result<Vec<Vec<f32>>, String> {
    Ok(texts.iter().map(|_| vec![0.5; usize::from(dim)]).collect())
}

fn scan(root: &Path, cfg: &ConfigV1) -> Vec<FileStatus> {
    let mut store = Store::open(root, VIEW_WORKING).expect("open store");
    let mut observer = CollectObserver::new();
    scan_with_observer(
        root,
        &mut store,
        cfg,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        &ScanCancel::new(),
        &mut observer,
        basemind::retrieval_wiring::backend(),
    )
    .expect("scan");
    observer
        .results()
        .iter()
        .filter(|result| result.path == "guide.md")
        .map(|result| result.status.clone())
        .collect()
}

fn entry(root: &Path) -> (bool, bool) {
    let store = Store::open(root, VIEW_WORKING).expect("open store");
    let entry = store.lookup_doc("guide.md").expect("guide.md is tracked");
    (entry.embedded, entry.embed_attempted)
}

#[test]
fn a_document_that_failed_to_embed_is_embedded_once_the_embedder_works() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(
        root.join("guide.md"),
        "# Pumps\n\nThe pump station lifts water uphill.\n",
    )
    .unwrap();
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.embed = true;

    test_document_embedder::set(Some(down));
    let first = scan(root, &cfg);
    assert!(
        first.iter().any(|s| matches!(s, FileStatus::DocIndexed { .. })),
        "indexed without vectors: {first:?}"
    );
    assert_eq!(entry(root), (false, false), "the document owes its vectors");

    let offline = scan(root, &cfg);
    assert!(
        offline.iter().all(|s| matches!(s, FileStatus::DocUnchanged)),
        "not re-extracted while the embedder is down: {offline:?}"
    );
    assert_eq!(entry(root), (false, false));

    test_document_embedder::set(Some(up));
    let online = scan(root, &cfg);
    test_document_embedder::set(None);
    assert!(
        online.iter().any(|s| matches!(s, FileStatus::DocIndexed { .. })),
        "re-extracted to pay the debt: {online:?}"
    );
    assert_eq!(entry(root), (true, true), "the document now has its vectors");
}
