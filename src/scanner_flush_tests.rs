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
        embed_failed: false,
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
                ..DocChunk::default()
            })
            .collect(),
        embedding_model: if dim > 0 {
            crate::embeddings::model_key(PRESET).into_owned()
        } else {
            String::new()
        },
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

/// The paths whose document rows are in the store with their vectors.
fn doc_rows(store: &mut Store, _scope: &str) -> Vec<String> {
    let Some(docs) = crate::scanner_docs::open_doc_store(store, &config(), false).expect("open") else {
        return Vec::new();
    };
    let keys = docs.keys().expect("keys");
    let embedded: ahash::AHashSet<String> = docs
        .documents(&keys)
        .expect("documents")
        .into_iter()
        .filter(|meta| meta.embedded && meta.n_chunks > 0)
        .map(|meta| meta.doc_key)
        .collect();
    let mut paths: Vec<String> = store
        .index
        .doc_files
        .iter()
        .filter(|(_, entry)| embedded.contains(&crate::lance::docs::doc_key(&entry.hash_hex, &entry.config_digest)))
        .map(|(path, _)| path.to_str_lossy().into_owned())
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

/// The directory of the group's document stores.
fn doc_stores_dir(store: &Store) -> std::path::PathBuf {
    crate::store_layout::group_index_dir(&store.root).join(crate::lance::docs::GROUP_STORE_DIR)
}

/// A file where the vector store's directory belongs: every open of the store fails.
fn block_vector_store(store: &Store) -> std::path::PathBuf {
    let dir = doc_stores_dir(store);
    std::fs::write(&dir, b"not a directory").unwrap();
    dir
}

/// A file where the code-chunk vector store's directory belongs: every open of it fails.
#[cfg(feature = "code-search")]
fn block_code_store(store: &Store) -> std::path::PathBuf {
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
/// scan: a blob re-extracted without vectors, a blob that is gone, a document the embed policy now
/// excludes. A document over the chunk cap keeps the vectors its blob already holds. The document
/// beside them still gets its rows.
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

    assert_eq!(doc_rows(&mut store, SCOPE), ["docs/good.txt", "docs/too-many-chunks.txt"]);
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
        chunk_count: 2,
        embedding_dim: DIM,
        emit_rows: true,
        embedded: true,
        embed_attempted: true,
        reused: true,
        degraded: false,
        text_fallback: false,
        over_chunk_cap: false,
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

/// A document whose rows could not be written stays owed, on disk, and the next pass writes it.
#[test]
fn rows_whose_write_failed_stay_owed_and_the_next_pass_writes_them() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    owe_doc(&mut store, "docs/good.txt", Some(&doc(2, DIM)));
    store.flush().unwrap();
    let blocker = block_vector_store(&store);

    lanes(&mut store, &config, usize::MAX);
    assert_eq!(
        owed_on_disk(&store),
        ["docs/good.txt"],
        "the failed write must stay owed"
    );

    std::fs::remove_file(&blocker).unwrap();
    lanes(&mut store, &config, usize::MAX);
    assert_eq!(doc_rows(&mut store, SCOPE), ["docs/good.txt"]);
    assert!(owed_on_disk(&store).is_empty());
}

/// The store is asked which documents it lacks (ADR-0029 rule 11) and a blob is read only for
/// those; an incremental pass writes a bounded number of them and leaves the rest owed.
#[test]
fn an_incremental_scan_writes_a_bounded_number_of_absent_documents() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    let total = REQUEUE_PER_INCREMENTAL + 6;
    for index in 0..total {
        owe_doc(
            &mut store,
            &format!("docs/{index:03}.txt"),
            Some(&doc(1 + index % 2, DIM)),
        );
    }

    lanes(&mut store, &config, REQUEUE_PER_INCREMENTAL);

    assert_eq!(doc_rows(&mut store, SCOPE).len(), REQUEUE_PER_INCREMENTAL);
    assert_eq!(store.pending_vectors.docs().len(), 6);
    assert_eq!(owed_on_disk(&store).len(), 6);

    lanes(&mut store, &config, usize::MAX);
    assert_eq!(doc_rows(&mut store, SCOPE).len(), total, "a full pass writes the rest");
    assert!(owed_on_disk(&store).is_empty());
}

fn inline_scan(root: &Path, store: &mut Store, config: &Config) {
    scan(root, store, config, ScanSource::WorkingTree, EmbedMode::Inline).unwrap();
}

/// The document store can be wiped or deleted on its own (`cache clear`, a new table version),
/// while the file map and the blobs still say every file is embedded. The next scan finds the
/// documents absent and writes them back from the blobs.
#[test]
fn a_vector_store_that_was_removed_gets_its_rows_back_from_the_next_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    let (rel, scope) = workspace_with_a_cached_document(tmp.path(), &store);
    inline_scan(tmp.path(), &mut store, &config);
    assert_eq!(doc_rows(&mut store, &scope), [rel]);

    std::fs::remove_dir_all(doc_stores_dir(&store)).unwrap();
    inline_scan(tmp.path(), &mut store, &config);

    assert_eq!(
        doc_rows(&mut store, &scope),
        [rel],
        "the unchanged file's rows are rewritten"
    );
    assert!(owed_on_disk(&store).is_empty());
}

/// The same loss found by an incremental scan of some other path.
#[test]
fn an_incremental_scan_also_finds_the_vector_store_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    let (rel, scope) = workspace_with_a_cached_document(tmp.path(), &store);
    inline_scan(tmp.path(), &mut store, &config);
    std::fs::remove_dir_all(doc_stores_dir(&store)).unwrap();

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

/// LanceDB commits are not fsynced by basemind. Rows lost with the machine are absent from the
/// store, and any later pass finds them absent and writes them again.
#[test]
fn rows_lost_with_the_machine_are_rewritten_by_the_next_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = config();
    let (rel, scope) = workspace_with_a_cached_document(tmp.path(), &store);
    inline_scan(tmp.path(), &mut store, &config);
    let docs = crate::scanner_docs::open_doc_store(&store, &config, false)
        .unwrap()
        .unwrap();
    docs.delete_keys(&docs.keys().unwrap()).unwrap();
    assert!(doc_rows(&mut store, &scope).is_empty(), "the rows are lost");

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
            extract_epoch: crate::extract::EXTRACT_EPOCH,
            embedding_dim: dim,
            embedding_model: if dim > 0 {
                crate::embeddings::model_key(PRESET).into_owned()
            } else {
                String::new()
            },
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
                content_key: None,
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

    /// A staged or revision view shares the working view's `code_chunks` store, keyed by path, so
    /// its scan must not write there: it would replace the working rows with other content.
    #[test]
    fn a_staged_view_writes_no_code_vectors() {
        let tmp = tempfile::tempdir().unwrap();
        crate::store::init_isolated_cache();
        let mut store = Store::open(tmp.path(), crate::store::VIEW_STAGED).unwrap();
        let config = code_config();
        owe_code(&mut store, "src/good.rs", Some(&sidecar(2, DIM)));
        store.flush().unwrap();

        lanes(&mut store, &config, usize::MAX);

        assert!(code_rows(&mut store).is_empty());
        assert_eq!(
            store.pending_vectors.code().len(),
            1,
            "owed, not written, until the working view"
        );
    }

    /// A chunk sidecar torn by a power loss still exists, so every later pass skipped its file as
    /// unchanged; settling the debt then left the file without rows for good. The debt stays, and the
    /// file map, on disk, sends the file to be extracted again.
    #[test]
    fn a_debt_whose_sidecar_is_torn_stays_owed_and_its_file_is_extracted_again() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let config = code_config();
        owe_code(&mut store, "src/torn.rs", Some(&sidecar(2, DIM)));
        let sidecar_path = store.blob_path_chunk_hex(&store.lookup("src/torn.rs").unwrap().hash_hex);
        let whole = std::fs::read(&sidecar_path).unwrap();
        std::fs::write(&sidecar_path, &whole[..whole.len() / 2]).unwrap();
        store.flush().unwrap();

        lanes(&mut store, &config, usize::MAX);

        let owed: Vec<String> = PendingVectors::load(&store.view_dir)
            .code()
            .iter()
            .map(|rel| rel.to_str_lossy().into_owned())
            .collect();
        assert_eq!(owed, ["src/torn.rs"], "the debt is kept");
        let on_disk = crate::store::read_index(&store.view_dir).unwrap().unwrap();
        let entry = on_disk
            .files
            .get(bstr::BStr::new(b"src/torn.rs"))
            .expect("still listed");
        assert_eq!(entry.extract_epoch, 0, "the next pass extracts it again");
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
        let blocker = block_code_store(&store);

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

    /// An owed set that cannot be read loads empty, but the paths it named are unknown, not none:
    /// every indexed file is owed again and its rows are written.
    #[test]
    fn an_unreadable_owed_set_owes_every_indexed_file_again() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(tmp.path());
        let config = code_config();
        owe_code(&mut store, "src/good.rs", Some(&sidecar(2, DIM)));
        store.flush().unwrap();
        std::fs::write(store.view_dir.join(PENDING_FILE), b"\xc1 torn").unwrap();
        store.pending_vectors = PendingVectors::load(&store.view_dir);
        assert!(store.pending_vectors.is_empty());

        lanes(&mut store, &config, usize::MAX);

        assert_eq!(code_rows(&mut store), ["src/good.rs"]);
        assert!(store.pending_vectors.is_empty());
        assert!(!store.view_dir.join(PENDING_FILE).exists(), "the torn file is replaced");
    }
}
