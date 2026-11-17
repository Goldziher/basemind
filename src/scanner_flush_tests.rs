use std::path::Path;

use super::*;
use crate::config::Config;
use crate::extract::doc::{DocBlobs, DocChunk, FileMapDoc};
use crate::scanner::{EmbedMode, ScanSource, scan, scan_paths};
use crate::scanner_flush::{REQUEUE_PER_INCREMENTAL, flush_vector_lanes};
use crate::store::{DocEntry, VIEW_WORKING};
use crate::store_pending::{PENDING_FILE, PendingVectors};

const PRESET: &str = "balanced";
const DIM: u16 = 768;
const SCOPE: &str = "repo:s";

fn config() -> Config {
    let mut config = crate::config::default_for_root(Path::new("."));
    config.documents.enabled = true;
    config.documents.embed = true;
    config.documents.embedding_preset = PRESET.to_string();
    config.code_search.enabled = false;
    config.code_search.embed = false;
    config
}

fn filters(config: &Config) -> Filters {
    Filters::build(config, Vec::new()).unwrap()
}

/// A document blob of `chunks` chunks, each with a vector of `dim` (none when `dim` is 0).
fn doc(chunks: usize, dim: u16) -> FileMapDoc {
    FileMapDoc {
        config_digest: String::new(),
        linear_chunked: false,
        text_fallback: false,
        schema_ver: crate::extract::SCHEMA_VER,
        mime_type: "text/plain".to_string(),
        content: "body".to_string(),
        metadata: Vec::new(),
        detected_languages: Vec::new(),
        chunks: (0..chunks)
            .map(|i| DocChunk {
                byte_start: i as u32,
                byte_end: i as u32 + 1,
                text: format!("chunk {i}"),
                embedding: vec![0.25_f32; usize::from(dim)],
            })
            .collect(),
        embedding_model: if dim > 0 { PRESET.to_string() } else { String::new() },
        embedding_dim: dim,
        keywords: Vec::new(),
        entities: Vec::new(),
        summary: None,
        language_confidences: Vec::new(),
    }
}

/// Index `rel` as a document whose blob is `blob` (no blob at all for `None`) and record it as
/// owed rows, the state a scan leaves when its vector write did not land.
fn owe_doc(store: &mut Store, rel: &str, blob: Option<&FileMapDoc>) {
    let hash = crate::hashing::hash_bytes(rel.as_bytes());
    if let Some(blob) = blob {
        store.write_doc(&hash, blob).unwrap();
    }
    store.upsert_doc(
        rel,
        DocEntry {
            config_digest: String::new(),
            hash_hex: crate::hashing::hex(&hash),
            embedding_preset: PRESET.to_string(),
            size_bytes: 1,
            mtime: 0,
            embedded: true,
            embed_attempted: true,
        },
    );
    store.pending_vectors.owe_doc(rel);
}

fn doc_rows(store: &mut Store, scope: &str) -> Vec<String> {
    let lance = store.lance_or_open(DIM, PRESET).expect("open lance").clone();
    let mut paths: Vec<String> = lance
        .search_documents(scope, vec![0.25; usize::from(DIM)], 500, None)
        .expect("search")
        .into_iter()
        .map(|hit| hit.path)
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn owed_on_disk(store: &Store) -> Vec<String> {
    let mut owed: Vec<String> = PendingVectors::load(&store.view_dir)
        .docs()
        .iter()
        .map(|rel| rel.to_str_lossy().into_owned())
        .collect();
    owed.sort();
    owed
}

fn open_store(root: &Path) -> Store {
    crate::store::init_isolated_cache();
    Store::open(root, VIEW_WORKING).unwrap()
}

/// A file where the vector store's directory belongs: every open of the store fails.
fn block_vector_store(store: &Store) -> std::path::PathBuf {
    let lance_dir = store.basemind_dir.join(crate::store::LANCE_DIR);
    std::fs::write(&lance_dir, b"not a directory").unwrap();
    lance_dir
}

fn lanes(store: &mut Store, config: &Config, requeue: usize) {
    flush_vector_lanes(store, config, &filters(config), SCOPE, Vec::new(), Vec::new(), requeue).unwrap();
}

/// A workspace with one document whose blob is already cached with vectors, so a scan indexes and
/// embeds it without running a model. Returns the document's path and the scan scope.
fn workspace_with_a_cached_document(root: &Path, store: &Store) -> (&'static str, String) {
    let body = b"a note that is long enough to be a document\n";
    std::fs::write(root.join("notes.txt"), body).unwrap();
    store
        .write_doc(&crate::hashing::hash_bytes(body), &doc(2, DIM))
        .unwrap();
    ("notes.txt", format!("path:{}", root.display()))
}

/// The file map is flushed before the vector lanes run, so an entry that reads as embedded can sit
/// over rows that never landed (a killed process, a LanceDB error). The scan itself must record the
/// path as owed, on disk, and the next scan, which has no batch of its own for an unchanged file,
/// must write the rows from the cached blob.
#[test]
fn rows_a_scan_could_not_write_stay_owed_and_the_next_scan_writes_them() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    let (rel, scope) = workspace_with_a_cached_document(tmp.path(), &store);
    let blocker = block_vector_store(&store);

    scan(
        tmp.path(),
        &mut store,
        &config,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
    )
    .unwrap();

    assert!(
        store.lookup_doc(rel).is_some_and(|entry| entry.embedded),
        "the file map claims the file"
    );
    assert_eq!(owed_on_disk(&store), [rel], "and the scan recorded its rows as owed");

    std::fs::remove_file(&blocker).unwrap();
    store.lance = None;
    scan(
        tmp.path(),
        &mut store,
        &config,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
    )
    .unwrap();

    assert_eq!(
        doc_rows(&mut store, &scope),
        [rel],
        "the next scan writes the owed rows"
    );
    assert!(store.pending_vectors.is_empty());
    assert!(!store.view_dir.join(PENDING_FILE).exists(), "nothing owed, no file");
}

/// An incremental scan of some other path settles what an earlier scan left owing.
#[test]
fn an_incremental_scan_writes_rows_an_earlier_scan_left_owed() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    let (rel, scope) = workspace_with_a_cached_document(tmp.path(), &store);
    let blocker = block_vector_store(&store);
    scan(
        tmp.path(),
        &mut store,
        &config,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
    )
    .unwrap();
    assert_eq!(owed_on_disk(&store), [rel]);
    std::fs::remove_file(&blocker).unwrap();
    store.lance = None;

    let other = tmp.path().join("lib.rs");
    std::fs::write(&other, "fn other() {}\n").unwrap();
    scan_paths(tmp.path(), &mut store, &config, &[other], EmbedMode::Inline).unwrap();

    assert_eq!(doc_rows(&mut store, &scope), [rel]);
    assert!(owed_on_disk(&store).is_empty());
}

/// Settling writes the small owed file, never the file map: the map on disk is not rewritten.
#[test]
fn settling_owed_rows_does_not_rewrite_the_file_map() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    owe_doc(&mut store, "docs/a.txt", Some(&doc(2, DIM)));
    store.flush().unwrap();
    assert_eq!(
        owed_on_disk(&store),
        ["docs/a.txt"],
        "the flush writes the owed set with the map"
    );
    let map = store.view_dir.join(crate::store::INDEX_FILE);
    let generation = store.index.generation;
    let written = std::fs::metadata(&map).unwrap().modified().unwrap();

    lanes(&mut store, &config, usize::MAX);

    assert_eq!(doc_rows(&mut store, SCOPE), ["docs/a.txt"]);
    assert!(owed_on_disk(&store).is_empty());
    assert_eq!(store.index.generation, generation, "no second seal of the index");
    assert_eq!(std::fs::metadata(&map).unwrap().modified().unwrap(), written);
}

/// The owed set is written before the file map that claims its files. When it cannot be written
/// the map must not be, or a crash would leave a map that vouches for rows nothing records as owed.
#[test]
fn the_owed_set_is_written_before_the_file_map_that_claims_its_files() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    store.flush().unwrap();
    let map = store.view_dir.join(crate::store::INDEX_FILE);
    let before = std::fs::read(&map).unwrap();
    owe_doc(&mut store, "docs/a.txt", Some(&doc(2, DIM)));
    let temp = format!("{PENDING_FILE}.{}.tmp", std::process::id());
    std::fs::create_dir(store.view_dir.join(temp)).unwrap();

    assert!(store.flush().is_err(), "the owed set cannot be written");

    assert_eq!(
        std::fs::read(&map).unwrap(),
        before,
        "the file map was written although the owed set was not"
    );
    assert!(!store.view_dir.join(PENDING_FILE).exists());
}

/// A path no rows can come of leaves the owed set instead of failing its whole batch on every
/// scan: a blob re-extracted without vectors, a blob that is gone, a document the embed policy or
/// the chunk cap now excludes. The document beside them still gets its rows.
#[test]
fn a_document_no_rows_can_come_of_is_settled_and_does_not_fail_the_others() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let mut config = config();
    config.documents.embed_exclude = vec!["excluded/**".to_string()];
    config.documents.max_chunks_per_document = 3;
    owe_doc(&mut store, "docs/good.txt", Some(&doc(2, DIM)));
    owe_doc(&mut store, "docs/vectorless.txt", Some(&doc(2, 0)));
    owe_doc(&mut store, "docs/other-dim.txt", Some(&doc(2, 384)));
    owe_doc(&mut store, "docs/blobless.txt", None);
    owe_doc(&mut store, "docs/too-many-chunks.txt", Some(&doc(4, DIM)));
    owe_doc(&mut store, "excluded/a.txt", Some(&doc(2, DIM)));
    store.pending_vectors.owe_doc("docs/not-indexed.txt");

    lanes(&mut store, &config, usize::MAX);

    assert_eq!(doc_rows(&mut store, SCOPE), ["docs/good.txt"]);
    assert!(store.pending_vectors.is_empty(), "{:?}", store.pending_vectors);
}

/// The batch a scan hands over for a document with no rows to write is skipped by the lane, so the
/// lane cannot settle it; and a blob whose vectors are of another dimension must not reach the
/// write, where it would fail every document batched with it.
#[test]
fn a_batch_whose_blob_holds_other_vectors_is_settled_not_written() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    owe_doc(&mut store, "docs/good.txt", Some(&doc(2, DIM)));
    owe_doc(&mut store, "docs/other-dim.txt", Some(&doc(2, 384)));
    let batch = |rel: &str| crate::scan_retrieval::PendingDocBatch {
        rel_path: rel.to_string(),
        blob_hash: crate::hashing::hex(&crate::hashing::hash_bytes(rel.as_bytes())),
        doc_scope: SCOPE.to_string(),
        chunk_count: 2,
        embedding_dim: DIM,
        emit_rows: true,
        embedded: true,
        embed_attempted: true,
        reused: true,
        degraded: false,
        text_fallback: false,
    };
    let batches = vec![batch("docs/good.txt"), batch("docs/other-dim.txt")];

    flush_vector_lanes(
        &mut store,
        &config,
        &filters(&config),
        SCOPE,
        batches,
        Vec::new(),
        usize::MAX,
    )
    .unwrap();

    assert_eq!(doc_rows(&mut store, SCOPE), ["docs/good.txt"]);
    assert!(store.pending_vectors.is_empty(), "{:?}", store.pending_vectors);
}

/// While the vector store cannot be opened nothing is looked at: a path that would be settled on
/// sight (its blob is gone) is still owed afterwards.
#[test]
fn nothing_is_requeued_while_the_vector_store_cannot_be_opened() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    owe_doc(&mut store, "docs/blobless.txt", None);
    let blocker = block_vector_store(&store);

    lanes(&mut store, &config, usize::MAX);
    assert_eq!(
        store.pending_vectors.docs().len(),
        1,
        "untouched while the store is unavailable"
    );

    std::fs::remove_file(&blocker).unwrap();
    // A failed open is remembered for the rest of the pass; the next pass's preflight retries it.
    store.retry_vector_store();
    lanes(&mut store, &config, usize::MAX);
    assert!(store.pending_vectors.is_empty());
}

/// A write that fails once the store is open leaves the document owed, on disk, for the next pass.
#[test]
fn rows_whose_write_failed_stay_owed_and_the_next_pass_writes_them() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    owe_doc(&mut store, "docs/good.txt", Some(&doc(2, DIM)));
    store.flush().unwrap();
    let lance = store.lance_or_open(DIM, PRESET).unwrap().clone();
    std::fs::remove_dir_all(lance.dir().join("documents.lance")).unwrap();

    lanes(&mut store, &config, usize::MAX);

    assert_eq!(
        owed_on_disk(&store),
        ["docs/good.txt"],
        "the failed write must stay owed"
    );
    assert_eq!(store.pending_vectors.docs().len(), 1);

    drop(lance);
    store.lance = None;
    lanes(&mut store, &config, usize::MAX);
    assert_eq!(doc_rows(&mut store, SCOPE), ["docs/good.txt"]);
    assert!(owed_on_disk(&store).is_empty());
}

/// An incremental scan re-queues a bounded number of owed paths and leaves the rest owed.
#[test]
fn an_incremental_scan_requeues_a_bounded_number_of_owed_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    let blob = doc(1, DIM);
    let total = REQUEUE_PER_INCREMENTAL + 6;
    for index in 0..total {
        owe_doc(&mut store, &format!("docs/{index:03}.txt"), Some(&blob));
    }

    lanes(&mut store, &config, REQUEUE_PER_INCREMENTAL);

    assert_eq!(doc_rows(&mut store, SCOPE).len(), REQUEUE_PER_INCREMENTAL);
    assert_eq!(store.pending_vectors.docs().len(), 6);
    assert_eq!(owed_on_disk(&store).len(), 6);
}

fn inline_scan(root: &Path, store: &mut Store, config: &Config) {
    scan(root, store, config, ScanSource::WorkingTree, EmbedMode::Inline).unwrap();
}

/// The vector store can be wiped or deleted on its own (another model opened it, `cache clear`,
/// an unreadable `meta.json`), while the file map and the blobs still say every file is embedded.
/// The scan that finds another store where the one it wrote to was puts the rows back.
#[test]
fn a_vector_store_that_was_removed_gets_its_rows_back_from_the_next_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    let (rel, scope) = workspace_with_a_cached_document(tmp.path(), &store);
    inline_scan(tmp.path(), &mut store, &config);
    assert_eq!(doc_rows(&mut store, &scope), [rel]);
    let lance_dir = store.basemind_dir.join(crate::store::LANCE_DIR);
    let first = crate::lance::store_id(&lance_dir).expect("the store has an id");
    assert_eq!(
        crate::store_pending::recorded_store(&store.view_dir).as_deref(),
        Some(first.as_str()),
        "the scan recorded the store its rows went to"
    );

    store.lance = None;
    std::fs::remove_dir_all(&lance_dir).unwrap();
    inline_scan(tmp.path(), &mut store, &config);

    assert_eq!(
        doc_rows(&mut store, &scope),
        [rel],
        "the unchanged file's rows are rewritten"
    );
    assert!(owed_on_disk(&store).is_empty());
    let second = crate::lance::store_id(&lance_dir).expect("the new store has an id");
    assert_ne!(first, second);
    assert_eq!(
        crate::store_pending::recorded_store(&store.view_dir).as_deref(),
        Some(second.as_str())
    );
}

/// The same loss found by an incremental scan of some other path.
#[test]
fn an_incremental_scan_also_finds_the_vector_store_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    let (rel, scope) = workspace_with_a_cached_document(tmp.path(), &store);
    inline_scan(tmp.path(), &mut store, &config);
    store.lance = None;
    std::fs::remove_dir_all(store.basemind_dir.join(crate::store::LANCE_DIR)).unwrap();

    let other = tmp.path().join("lib.rs");
    std::fs::write(&other, "fn other() {}\n").unwrap();
    scan_paths(tmp.path(), &mut store, &config, &[other], EmbedMode::Inline).unwrap();

    assert_eq!(doc_rows(&mut store, &scope), [rel]);
}

/// A view whose scans never opened a vector store has no rows anywhere: a store that is not
/// there is not a lost one, and nothing is owed or created for it.
#[test]
fn a_view_that_never_wrote_rows_owes_nothing_for_a_store_that_is_not_there() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    workspace_with_a_cached_document(tmp.path(), &store);

    scan(
        tmp.path(),
        &mut store,
        &config,
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
    )
    .unwrap();
    lanes(&mut store, &config, usize::MAX);

    assert_eq!(crate::store_pending::recorded_store(&store.view_dir), None);
    assert!(store.pending_vectors.is_empty());
    assert!(!store.basemind_dir.join(crate::store::LANCE_DIR).exists());
}

/// LanceDB commits are not fsynced by basemind, and a path's debt is settled once its commit
/// returns. After the machine went down those rows may be gone with nothing owed. The pass that
/// verifies blobs owes every path its rows again; an ordinary pass does not look.
#[test]
fn the_pass_that_verifies_blobs_rewrites_vector_rows_lost_with_the_machine() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    let (rel, scope) = workspace_with_a_cached_document(tmp.path(), &store);
    inline_scan(tmp.path(), &mut store, &config);
    let lance = store.lance_or_open(DIM, PRESET).unwrap().clone();
    lance.replace_document(&scope, rel, Vec::new()).unwrap();
    drop(lance);
    assert!(doc_rows(&mut store, &scope).is_empty(), "the rows are lost");

    inline_scan(tmp.path(), &mut store, &config);
    assert!(
        doc_rows(&mut store, &scope).is_empty(),
        "an ordinary pass trusts the file map"
    );

    std::mem::forget(crate::scan_evidence::ScanBreadcrumb::begin_in(&store.basemind_dir, tmp.path()).unwrap());
    inline_scan(tmp.path(), &mut store, &config);

    assert_eq!(doc_rows(&mut store, &scope), [rel]);
    assert!(owed_on_disk(&store).is_empty());
}

/// A view an earlier build last scanned records no vector store. The pass that verifies blobs owes
/// its rows all the same: they were written with no fsync either way.
#[test]
fn the_verifying_pass_rewrites_lost_rows_of_a_view_that_records_no_store() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    let (rel, scope) = workspace_with_a_cached_document(tmp.path(), &store);
    inline_scan(tmp.path(), &mut store, &config);
    let lance = store.lance_or_open(DIM, PRESET).unwrap().clone();
    lance.replace_document(&scope, rel, Vec::new()).unwrap();
    drop(lance);
    std::fs::remove_file(store.view_dir.join(crate::store_pending::STORE_ID_FILE)).unwrap();
    assert!(doc_rows(&mut store, &scope).is_empty(), "the rows are lost");

    std::mem::forget(crate::scan_evidence::ScanBreadcrumb::begin_in(&store.basemind_dir, tmp.path()).unwrap());
    inline_scan(tmp.path(), &mut store, &config);

    assert_eq!(doc_rows(&mut store, &scope), [rel]);
    assert!(owed_on_disk(&store).is_empty());
}

/// A full scan that is cancelled takes an incremental pass's share of what earlier scans left
/// owing, not all of it: the daemon cancels its scans to stop, and must not then re-read every
/// owed blob.
#[test]
fn a_cancelled_full_scan_requeues_a_bounded_number_of_owed_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    let blob = doc(1, DIM);
    let total = REQUEUE_PER_INCREMENTAL + 6;
    for index in 0..total {
        owe_doc(&mut store, &format!("docs/{index:03}.txt"), Some(&blob));
    }
    let cancel = crate::scanner::ScanCancel::new();
    cancel.cancel();

    let report = crate::scanner::scan_with_cancel(
        tmp.path(),
        &mut store,
        &config,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        &cancel,
    )
    .unwrap();

    assert!(report.cancelled);
    assert_eq!(store.pending_vectors.docs().len(), 6, "the rest stays owed");
}

/// A store opened without the workspace lock and scanned in place loaded the owed set at open.
/// What another process recorded since must survive this scan writing the set.
#[test]
fn a_store_scanned_in_place_keeps_what_another_process_recorded_as_owed() {
    let tmp = tempfile::tempdir().unwrap();
    let config = config();
    let (rel, _scope) = {
        let writer = open_store(tmp.path());
        writer.ensure_dirs().unwrap();
        workspace_with_a_cached_document(tmp.path(), &writer)
    };
    let mut in_place = Store::open_read_only(tmp.path(), VIEW_WORKING).unwrap();
    assert!(in_place.pending_vectors.is_empty());
    let mut other_process = PendingVectors::default();
    other_process.owe_doc("docs/theirs.txt");
    other_process.persist(&in_place.view_dir).unwrap();
    // The scan's own rows cannot be written, so it has a debt of its own to record.
    block_vector_store(&in_place);

    inline_scan(tmp.path(), &mut in_place, &config);

    assert_eq!(owed_on_disk(&in_place), ["docs/theirs.txt", rel]);
}

#[cfg(feature = "code-search")]
mod code {
    use super::*;
    use crate::chunk::{CodeChunk, CodeChunkBlob};
    use crate::store::FileEntry;

    fn code_config() -> Config {
        let mut config = config();
        config.code_search.enabled = true;
        config.code_search.embed = true;
        config
    }

    fn sidecar(chunks: usize, dim: u16) -> CodeChunkBlob {
        CodeChunkBlob {
            config_digest: String::new(),
            schema_ver: crate::extract::SCHEMA_VER,
            embedding_dim: dim,
            embedding_model: if dim > 0 { PRESET.to_string() } else { String::new() },
            chunks: (0..chunks)
                .map(|i| CodeChunk {
                    chunk_id: format!("h:{i}"),
                    path: "src/lib.rs".to_string(),
                    lang: "rust".to_string(),
                    kind: None,
                    symbol: None,
                    signature: None,
                    doc: None,
                    byte_start: 0,
                    byte_end: 0,
                    line_start: 1,
                    line_end: 1,
                    text: "fn a() {}".to_string(),
                    searchable_text: "fn a".to_string(),
                })
                .collect(),
            embeddings: if dim > 0 {
                vec![vec![0.25_f32; usize::from(dim)]; chunks]
            } else {
                Vec::new()
            },
        }
    }

    fn owe_code(store: &mut Store, rel: &str, blob: Option<&CodeChunkBlob>) {
        let hash_hex = crate::hashing::hex(&crate::hashing::hash_bytes(rel.as_bytes()));
        if let Some(blob) = blob {
            store.write_chunks_hex(&hash_hex, blob).unwrap();
        }
        store.upsert(
            rel,
            FileEntry {
                hash_hex,
                language: "rust".to_string(),
                size_bytes: 1,
                mtime: 0,
                extract_epoch: crate::extract::EXTRACT_EPOCH,
            },
        );
        store.pending_vectors.owe_code(rel);
    }

    fn code_rows(store: &mut Store) -> Vec<String> {
        let lance = store.lance_or_open(DIM, PRESET).expect("open lance").clone();
        let mut paths: Vec<String> = lance
            .search_code_chunks(SCOPE, vec![0.25; usize::from(DIM)], 500)
            .expect("search")
            .into_iter()
            .map(|hit| hit.path)
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// The code lane: an owed file's rows are rebuilt from its chunk sidecar, and a file no rows
    /// can come of (sidecar gone, sidecar without vectors, embed policy off for it, over the chunk
    /// cap, no longer indexed) leaves the owed set.
    #[test]
    fn owed_code_rows_are_written_from_the_sidecar_and_unbuildable_ones_settled() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let mut config = code_config();
        config.code_search.embed_exclude = vec!["excluded/**".to_string()];
        config.code_search.max_chunks_per_file = 3;
        owe_code(&mut store, "src/good.rs", Some(&sidecar(2, DIM)));
        owe_code(&mut store, "src/vectorless.rs", Some(&sidecar(2, 0)));
        owe_code(&mut store, "src/sidecarless.rs", None);
        owe_code(&mut store, "src/too-many-chunks.rs", Some(&sidecar(4, DIM)));
        owe_code(&mut store, "excluded/a.rs", Some(&sidecar(2, DIM)));
        store.pending_vectors.owe_code("src/not-indexed.rs");
        store.flush().unwrap();
        assert_eq!(PendingVectors::load(&store.view_dir).code().len(), 6);

        lanes(&mut store, &config, usize::MAX);

        assert_eq!(code_rows(&mut store), ["src/good.rs"]);
        assert!(store.pending_vectors.is_empty(), "{:?}", store.pending_vectors);
        assert!(!store.view_dir.join(PENDING_FILE).exists());
    }

    /// The batch a scan hands over for a file whose sidecar is gone, or holds vectors of another
    /// dimension, is settled by the lane instead of written: the write would fail every file
    /// batched with it, and nothing can come of the file until it is chunked again.
    #[test]
    fn a_code_batch_whose_sidecar_is_gone_or_holds_other_vectors_is_settled_not_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let config = code_config();
        owe_code(&mut store, "src/good.rs", Some(&sidecar(2, DIM)));
        owe_code(&mut store, "src/other-dim.rs", Some(&sidecar(2, 384)));
        owe_code(&mut store, "src/sidecarless.rs", None);
        let batch = |store: &Store, rel: &str| crate::scanner_code::PendingCodeBatch {
            rel_path: rel.to_string(),
            blob_hash: store.lookup(rel).unwrap().hash_hex.clone(),
            embedding_dim: DIM,
            bm25: Vec::new(),
        };
        let batches = ["src/good.rs", "src/other-dim.rs", "src/sidecarless.rs"]
            .map(|rel| batch(&store, rel))
            .to_vec();

        flush_vector_lanes(
            &mut store,
            &config,
            &filters(&config),
            SCOPE,
            Vec::new(),
            batches,
            usize::MAX,
        )
        .unwrap();

        assert_eq!(code_rows(&mut store), ["src/good.rs"]);
        assert!(store.pending_vectors.is_empty(), "{:?}", store.pending_vectors);
    }

    /// A write that fails once the store is open leaves the code file owed.
    #[test]
    fn code_rows_whose_write_failed_stay_owed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let config = code_config();
        owe_code(&mut store, "src/good.rs", Some(&sidecar(2, DIM)));
        let batch = crate::scanner_code::PendingCodeBatch {
            rel_path: "src/good.rs".to_string(),
            blob_hash: store.lookup("src/good.rs").unwrap().hash_hex.clone(),
            embedding_dim: DIM,
            bm25: Vec::new(),
        };
        store.flush().unwrap();
        let lance = store.lance_or_open(DIM, PRESET).unwrap().clone();
        std::fs::remove_dir_all(lance.dir().join("code_chunks.lance")).unwrap();

        flush_vector_lanes(
            &mut store,
            &config,
            &filters(&config),
            SCOPE,
            Vec::new(),
            vec![batch],
            usize::MAX,
        )
        .unwrap();

        assert_eq!(
            PendingVectors::load(&store.view_dir).code().len(),
            1,
            "still owed on disk"
        );
        assert_eq!(store.pending_vectors.code().len(), 1);
    }

    /// A failed write leaves the code file owed, on disk, and the next pass writes it.
    #[test]
    fn code_rows_that_could_not_be_written_stay_owed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let config = code_config();
        owe_code(&mut store, "src/good.rs", Some(&sidecar(2, DIM)));
        let batch = crate::scanner_code::PendingCodeBatch {
            rel_path: "src/good.rs".to_string(),
            blob_hash: store.lookup("src/good.rs").unwrap().hash_hex.clone(),
            embedding_dim: DIM,
            bm25: Vec::new(),
        };
        store.flush().unwrap();
        let blocker = block_vector_store(&store);

        flush_vector_lanes(
            &mut store,
            &config,
            &filters(&config),
            SCOPE,
            Vec::new(),
            vec![batch],
            usize::MAX,
        )
        .unwrap();
        assert_eq!(
            PendingVectors::load(&store.view_dir).code().len(),
            1,
            "still owed on disk"
        );

        std::fs::remove_file(&blocker).unwrap();
        store.retry_vector_store();
        lanes(&mut store, &config, usize::MAX);
        assert_eq!(code_rows(&mut store), ["src/good.rs"]);
        assert!(store.pending_vectors.is_empty());
    }
}
