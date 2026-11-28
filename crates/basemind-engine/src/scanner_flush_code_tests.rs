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
            lines: 0,
        },
    );
    store.pending_vectors.owe_code(rel);
}

/// The paths of `store` whose content has rows in the code-chunk store.
fn code_rows(store: &mut Store) -> Vec<String> {
    let lance = store.lance_or_open(DIM, PRESET).expect("open lance").clone();
    let held = lance.code_content_hashes().expect("contents");
    let mut paths: Vec<String> = store
        .index
        .files
        .iter()
        .filter(|(_, entry)| held.contains(&entry.hash_hex))
        .map(|(path, _)| path.to_str_lossy().into_owned())
        .collect();
    paths.sort();
    paths
}

/// How many chunk rows the store holds for the contents of `rels` in `store`.
fn chunk_rows_of(store: &mut Store, rels: &[&str]) -> usize {
    let lance = store.lance_or_open(DIM, PRESET).expect("open lance").clone();
    let hashes: Vec<String> = rels
        .iter()
        .map(|rel| store.lookup(rel).expect("indexed").hash_hex.clone())
        .collect();
    lance
        .search_code_chunks(vec![0.25; usize::from(DIM)], 500, Some(hashes.as_slice()))
        .expect("search")
        .len()
}

/// Two worktrees of one repository: their stores are views of one group.
fn worktrees() -> (tempfile::TempDir, Store, Store) {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let (main, linked) = crate::testing::linked_worktrees(tmp.path());
    let a = Store::open(&main, VIEW_WORKING).unwrap();
    let b = Store::open(&linked, VIEW_WORKING).unwrap();
    (tmp, a, b)
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

/// Re-queuing owed code rows spends the pass's budget, which made a full pass look incremental to
/// the document lane: it skipped the sweep of dead document rows whenever code rows were owed.
#[test]
fn a_full_pass_that_owes_code_rows_still_sweeps_dead_document_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = code_config();
    owe_doc(&mut store, "docs/live.txt", Some(&doc(2, DIM)));
    owe_code(&mut store, "src/good.rs", Some(&sidecar(2, DIM)));
    store.flush().unwrap();
    let docs = crate::scanner_docs::open_doc_store(&store, &config, true)
        .unwrap()
        .expect("created");
    docs.write(&[crate::lance::docs_schema::DocRows {
        doc_key: "dead".to_string(),
        mime_type: "text/plain".to_string(),
        mime_group: "text".to_string(),
        title: None,
        keywords: Vec::new(),
        chunks: Vec::new(),
        ..Default::default()
    }])
    .unwrap();

    lanes(&mut store, &config, usize::MAX);

    assert_eq!(code_rows(&mut store), ["src/good.rs"], "the code row was requeued");
    assert!(
        !docs.keys().unwrap().contains(&"dead".to_string()),
        "the dead row is swept"
    );
}

/// The rows are keyed by content in the group's one store: worktrees holding the same bytes
/// write one set of rows, and each worktree's search sees the rows of the contents it holds.
#[test]
fn worktrees_holding_identical_content_write_one_set_of_chunk_rows() {
    let (_tmp, mut a, mut b) = worktrees();
    let config = code_config();
    owe_code(&mut a, "src/shared.rs", Some(&sidecar(2, DIM)));
    owe_code(&mut b, "src/shared.rs", Some(&sidecar(2, DIM)));
    owe_code(&mut b, "src/only_b.rs", Some(&sidecar(3, DIM)));
    a.flush().unwrap();
    b.flush().unwrap();

    lanes(&mut a, &config, usize::MAX);
    lanes(&mut b, &config, usize::MAX);

    let dir_of = |store: &mut Store| store.lance_or_open(DIM, PRESET).unwrap().dir().to_path_buf();
    assert_eq!(dir_of(&mut a), dir_of(&mut b), "one store for the group");
    assert_eq!(
        chunk_rows_of(&mut a, &["src/shared.rs"]),
        2,
        "written once, not once per view"
    );
    assert_eq!(chunk_rows_of(&mut b, &["src/shared.rs", "src/only_b.rs"]), 5);
    assert!(a.pending_vectors.is_empty() && b.pending_vectors.is_empty());
    let a_live = a.live_content_hashes(usize::MAX).unwrap();
    let lance = a.lance_or_open(DIM, PRESET).unwrap().clone();
    let query = vec![0.25; usize::from(DIM)];
    let hits = lance
        .search_code_chunks(query.clone(), 500, Some(a_live.as_slice()))
        .unwrap();
    assert_eq!(
        hits.len(),
        2,
        "a view's search reads only the contents its file map names"
    );
    assert_eq!(
        lance.search_code_chunks(query, 500, None).unwrap().len(),
        5,
        "unrestricted"
    );
    assert_eq!(a.live_content_hashes(0), None, "a view over the cap names no list");
}

/// Files of identical content in one scan are one set of rows, and each file's debt is settled.
#[test]
fn files_of_identical_content_in_one_pass_write_one_set_of_rows_and_settle_every_debt() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open_store(tmp.path());
    let config = code_config();
    owe_code(&mut store, "src/a.rs", Some(&sidecar(2, DIM)));
    let original = store.lookup("src/a.rs").unwrap().clone();
    store.upsert("src/copy.rs", original);
    store.pending_vectors.owe_code("src/copy.rs");
    store.flush().unwrap();

    lanes(&mut store, &config, usize::MAX);

    assert_eq!(code_rows(&mut store), ["src/a.rs", "src/copy.rs"]);
    assert_eq!(chunk_rows_of(&mut store, &["src/a.rs"]), 2);
    assert!(store.pending_vectors.is_empty(), "{:?}", store.pending_vectors);
}

/// A file removed from one worktree keeps its rows while another worktree holds its content;
/// they go with the last holder.
#[test]
fn removed_files_rows_stay_while_another_worktree_holds_the_content() {
    let (_tmp, mut a, mut b) = worktrees();
    let config = code_config();
    owe_code(&mut a, "src/shared.rs", Some(&sidecar(2, DIM)));
    owe_code(&mut b, "src/shared.rs", Some(&sidecar(2, DIM)));
    a.flush().unwrap();
    b.flush().unwrap();
    lanes(&mut a, &config, usize::MAX);
    lanes(&mut b, &config, usize::MAX);
    let hashes = vec![a.lookup("src/shared.rs").unwrap().hash_hex.clone()];

    a.remove("src/shared.rs");
    a.flush().unwrap();
    crate::scanner_flush::flush_code_removals_if_any(
        &mut a,
        &config,
        &filters(&config),
        &["src/shared.rs".to_string()],
    );
    let query = vec![0.25; usize::from(DIM)];
    let lance = b.lance_or_open(DIM, PRESET).unwrap().clone();
    assert_eq!(
        lance
            .search_code_chunks(query.clone(), 500, Some(hashes.as_slice()))
            .unwrap()
            .len(),
        2
    );

    b.remove("src/shared.rs");
    b.flush().unwrap();
    crate::scanner_flush::flush_code_removals_if_any(
        &mut b,
        &config,
        &filters(&config),
        &["src/shared.rs".to_string()],
    );
    assert!(
        lance
            .search_code_chunks(query, 500, Some(hashes.as_slice()))
            .unwrap()
            .is_empty()
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
