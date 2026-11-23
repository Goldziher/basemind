use super::{ScanSource, settle_cancelled_embed_progress, settle_embed_progress};
use crate::config::ConfigV1;
use crate::embed_progress;
use crate::scanner_filter::Filters;
use crate::store::{DocEntry, Store, VIEW_WORKING};

/// A document entry as a pass left it: `embedded` and `embed_attempted` as given.
fn doc_entry(embedded: bool, embed_attempted: bool) -> DocEntry {
    DocEntry {
        hash_hex: "00".repeat(32),
        embedding_preset: "balanced".to_string(),
        size_bytes: 64,
        mtime: 0,
        embedded,
        embed_attempted,
        config_digest: String::new(),
    }
}

/// The store and config of a workspace tracking one document left as given, with a record
/// of an embedding pass that had paid 2 of 3 documents.
fn workspace_with(entry: DocEntry) -> (tempfile::TempDir, Store, ConfigV1) {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(tmp.path(), VIEW_WORKING).expect("open");
    store.upsert_doc("guide.md", entry);
    let mut config = ConfigV1::with_defaults();
    config.documents.enabled = true;
    config.documents.embed = true;
    embed_progress::record_owed(&crate::store_layout::workspace_cache_dir(tmp.path()), 3);
    (tmp, store, config)
}

/// The record's `total` once the pass settled, `None` when it was removed.
fn settled_total(
    entry: DocEntry,
    settle: fn(&std::path::Path, &Store, &ConfigV1, &Filters, &ScanSource<'_>),
) -> Option<usize> {
    let (tmp, store, config) = workspace_with(entry);
    let filters = Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("filters");
    settle(tmp.path(), &store, &config, &filters, &ScanSource::WorkingTree);
    embed_progress::read(&crate::store_layout::workspace_cache_dir(tmp.path())).map(|record| record.total)
}

/// An `Inline` pass that could not pay a document (no embedder, a spent budget) leaves it neither
/// embedded nor attempted: the record stays, naming it, so `index_status` keeps reporting it and a
/// restarted process resumes it. A paid or attempted document owes nothing, and the record goes.
#[test]
fn a_finished_pass_keeps_the_record_while_a_document_still_owes_its_vectors() {
    for (embedded, embed_attempted, expected) in [(false, false, Some(1)), (true, true, None), (false, true, None)] {
        assert_eq!(
            settled_total(doc_entry(embedded, embed_attempted), settle_embed_progress),
            expected,
            "embedded={embedded} embed_attempted={embed_attempted}"
        );
    }
}

/// A cancelled pass removes the record only when nothing is owed, and otherwise leaves it exactly as
/// the pass last wrote it.
#[test]
fn a_cancelled_pass_removes_the_record_only_once_nothing_is_owed() {
    for (embedded, embed_attempted, expected) in [(false, false, Some(3)), (true, true, None)] {
        assert_eq!(
            settled_total(doc_entry(embedded, embed_attempted), settle_cancelled_embed_progress),
            expected,
            "embedded={embedded} embed_attempted={embed_attempted}"
        );
    }
}
