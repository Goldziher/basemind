use crate::config::ConfigV1;
use crate::embed_progress;
use crate::scanner::{EmbedMode, ScanSource, scan};
use crate::store::{Store, VIEW_WORKING};

fn config(embed: bool) -> ConfigV1 {
    let mut config = ConfigV1::with_defaults();
    config.documents.enabled = true;
    config.documents.embed = embed;
    config
}

fn scan_in(root: &std::path::Path, store: &mut Store, config: &ConfigV1, mode: EmbedMode) {
    scan(
        root,
        store,
        config,
        ScanSource::WorkingTree,
        mode,
        crate::scanner_vectors::test_retrieval(),
    )
    .expect("scan");
}

fn guide_workspace() -> tempfile::TempDir {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("guide.md"),
        "# Guide\n\nHow the harbour is run, long enough to index as a document.\n",
    )
    .expect("guide.md");
    tmp
}

/// The fast pass tracks a document whose vectors are still to come, so it is found by its text at
/// once, and records how many documents owe vectors: the mark a restarted process resumes from.
#[test]
fn a_deferred_pass_tracks_documents_owing_vectors_and_records_the_debt() {
    let tmp = guide_workspace();
    let mut store = Store::open(tmp.path(), VIEW_WORKING).expect("open");
    scan_in(tmp.path(), &mut store, &config(true), EmbedMode::Deferred);

    let entry = store
        .lookup_doc("guide.md")
        .expect("tracked at the end of the fast pass");
    assert!(!entry.embedded && !entry.embed_attempted, "{entry:?}");
    let cache_dir = crate::store::workspace_cache_dir(tmp.path());
    let status = embed_progress::status(&cache_dir, embed_progress::now_unix_ms()).expect("the debt is recorded");
    assert_eq!((status.done, status.total, status.eta_secs), (0, 1, None));

    scan_in(tmp.path(), &mut store, &config(true), EmbedMode::Deferred);
    assert_eq!(
        embed_progress::read(&cache_dir).map(|record| record.total),
        Some(1),
        "a second fast pass finds the same debt"
    );
}

/// With embedding off nothing owes vectors: the fast pass records no debt, as before.
#[test]
fn a_deferred_pass_over_a_repository_that_does_not_embed_records_no_debt() {
    let tmp = guide_workspace();
    let mut store = Store::open(tmp.path(), VIEW_WORKING).expect("open");
    scan_in(tmp.path(), &mut store, &config(false), EmbedMode::Deferred);

    let cache_dir = crate::store::workspace_cache_dir(tmp.path());
    assert_eq!(embed_progress::read(&cache_dir), None);
}

/// An embedding pass that completes removes the record, and with it the report and the resume.
#[test]
fn a_completed_inline_pass_removes_the_record() {
    let tmp = guide_workspace();
    let mut store = Store::open(tmp.path(), VIEW_WORKING).expect("open");
    let cache_dir = crate::store::workspace_cache_dir(tmp.path());
    embed_progress::record_owed(&cache_dir, 3);

    scan_in(tmp.path(), &mut store, &config(false), EmbedMode::Inline);
    assert_eq!(embed_progress::read(&cache_dir), None);
}
