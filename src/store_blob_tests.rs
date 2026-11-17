use super::*;
use crate::store::{VIEW_WORKING, init_isolated_cache};

fn sample_l1() -> FileMapL1 {
    FileMapL1 {
        schema_ver: SCHEMA_VER,
        language: "rust".to_string(),
        size_bytes: 42,
        had_errors: false,
        error_count: 0,
        symbols: Vec::new(),
        imports: Vec::new(),
        implementations: Vec::new(),
        rationale: Vec::new(),
        extract_epoch: crate::extract::EXTRACT_EPOCH,
    }
}

fn sample_l2() -> FileMapL2 {
    FileMapL2 {
        schema_ver: SCHEMA_VER,
        language: "rust".to_string(),
        calls: Vec::new(),
        docs: Vec::new(),
    }
}

#[test]
fn filemap_frame_round_trips_both_tiers() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
    let hash_hex = "a".repeat(64);

    store
        .write_filemap_hex(&hash_hex, &sample_l1(), Some(&sample_l2()))
        .expect("write combined frame");

    let l1 = store.read_l1_by_hex(&hash_hex).expect("read l1");
    assert_eq!(l1.map(|m| m.size_bytes), Some(42), "L1 slice round-trips");
    let l2 = store.read_l2_by_hex(&hash_hex).expect("read l2");
    assert_eq!(l2.map(|m| m.language), Some("rust".to_string()), "L2 present");
}

#[test]
fn filemap_frame_l1_only_reads_back_no_l2() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
    let hash_hex = "b".repeat(64);

    store
        .write_filemap_hex(&hash_hex, &sample_l1(), None)
        .expect("write L1-only frame");

    assert!(
        store.read_l1_by_hex(&hash_hex).expect("read l1").is_some(),
        "L1 present in an L1-only frame"
    );
    assert!(
        store.read_l2_by_hex(&hash_hex).expect("read l2").is_none(),
        "L2 absent in an L1-only frame (escalation will extract on demand)"
    );
}

#[test]
fn new_filemap_blobs_use_a_compressed_self_describing_envelope() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
    let hash_hex = "c".repeat(64);
    let mut l1 = sample_l1();
    l1.language = "rust".repeat(1_024);

    store
        .write_filemap_hex(&hash_hex, &l1, Some(&sample_l2()))
        .expect("write compressed filemap");

    let path = store.blob_path_fm_hex(&hash_hex);
    let persisted = std::fs::read(&path).expect("read persisted filemap");
    let legacy = {
        let l1_bytes = rmp_serde::to_vec_named(&l1).expect("serialize legacy L1");
        let l2_bytes = rmp_serde::to_vec_named(&sample_l2()).expect("serialize legacy L2");
        let mut bytes = Vec::with_capacity(4 + l1_bytes.len() + l2_bytes.len());
        bytes.extend_from_slice(&(l1_bytes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&l1_bytes);
        bytes.extend_from_slice(&l2_bytes);
        bytes
    };

    assert_eq!(&persisted[..4], b"BMB1", "new blobs carry the envelope magic");
    assert!(
        persisted.len() < legacy.len(),
        "repetitive filemap payload should be smaller after compression"
    );
    assert_eq!(store.read_l1_by_hex(&hash_hex).unwrap(), Some(l1));
    assert_eq!(store.read_l2_by_hex(&hash_hex).unwrap(), Some(sample_l2()));
}

#[test]
fn legacy_uncompressed_filemap_blobs_remain_readable() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
    let hash_hex = "f".repeat(64);
    let l1 = sample_l1();
    let l2 = sample_l2();
    let l1_bytes = rmp_serde::to_vec_named(&l1).expect("serialize legacy L1");
    let l2_bytes = rmp_serde::to_vec_named(&l2).expect("serialize legacy L2");
    let mut legacy = Vec::with_capacity(4 + l1_bytes.len() + l2_bytes.len());
    legacy.extend_from_slice(&(l1_bytes.len() as u32).to_le_bytes());
    legacy.extend_from_slice(&l1_bytes);
    legacy.extend_from_slice(&l2_bytes);
    std::fs::write(store.blob_path_fm_hex(&hash_hex), legacy).expect("write legacy frame");

    assert_eq!(store.read_l1_by_hex(&hash_hex).unwrap(), Some(l1));
    assert_eq!(store.read_l2_by_hex(&hash_hex).unwrap(), Some(l2));
}

#[test]
fn filemap_l1_read_does_not_decompress_a_corrupt_l2_frame() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
    let hash_hex = "2".repeat(64);
    let l1 = sample_l1();
    store
        .write_filemap_hex(&hash_hex, &l1, Some(&sample_l2()))
        .expect("write compressed filemap");

    let path = store.blob_path_fm_hex(&hash_hex);
    let mut persisted = std::fs::read(&path).expect("read filemap bytes");
    let last = persisted.last_mut().expect("L2 compressed frame present");
    *last ^= 0xff;
    std::fs::write(&path, persisted).expect("corrupt only L2 frame");

    assert_eq!(store.read_l1_by_hex(&hash_hex).unwrap(), Some(l1));
    assert!(
        store.read_l2_by_hex(&hash_hex).is_err(),
        "corrupt L2 must fail when requested"
    );
}

#[test]
fn writing_a_current_filemap_repairs_a_corrupt_compressed_payload() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
    let hash_hex = "5".repeat(64);
    let l1 = sample_l1();
    let l2 = sample_l2();
    store
        .write_filemap_hex(&hash_hex, &l1, Some(&l2))
        .expect("write filemap");

    let path = store.blob_path_fm_hex(&hash_hex);
    let mut persisted = std::fs::read(&path).expect("read filemap bytes");
    persisted[FILEMAP_HEADER_LEN] ^= 0xff;
    std::fs::write(&path, persisted).expect("corrupt compressed L1");

    store
        .write_filemap_hex(&hash_hex, &l1, Some(&l2))
        .expect("repair corrupt filemap");
    assert_eq!(store.read_l1_by_hex(&hash_hex).unwrap(), Some(l1));
    assert_eq!(store.read_l2_by_hex(&hash_hex).unwrap(), Some(l2));
}

#[test]
fn resolved_blob_round_trips_and_missing_reads_none() {
    use crate::intel::model::{ExportEdge, FileResolvedRefs, ImportEdge, ResolvedEdge};
    init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
    let hash_hex = "d".repeat(64);

    let mut refs = FileResolvedRefs::new("typescript");
    refs.intra.push(ResolvedEdge {
        use_start: 40,
        use_end: 43,
        def_start: 4,
        def_end: 7,
    });
    refs.imports.push(ImportEdge {
        local: "foo".to_string(),
        specifier: "./bar".to_string(),
        imported: Some("baz".to_string()),
        is_type: false,
        local_start: 9,
    });
    refs.exports.push(ExportEdge {
        name: "alpha".to_string(),
        name_start: 20,
    });

    store.write_resolved_hex(&hash_hex, &refs).expect("write resolved blob");
    let persisted = std::fs::read(store.blob_path_rref_hex(&hash_hex)).expect("read resolved blob bytes");
    assert_eq!(
        &persisted[..4],
        b"BMB1",
        "new single-map blobs carry the envelope magic"
    );
    let read = store.read_resolved_by_hex(&hash_hex).expect("read resolved blob");
    assert_eq!(read.as_ref(), Some(&refs), "resolution blob round-trips exactly");

    let missing = store.read_resolved_by_hex(&"e".repeat(64)).expect("read missing");
    assert_eq!(missing, None, "absent resolution blob reads back as None");
}

#[test]
fn legacy_uncompressed_single_map_blobs_remain_readable() {
    use crate::intel::model::FileResolvedRefs;

    init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
    let hash_hex = "1".repeat(64);
    let refs = FileResolvedRefs::new("rust");
    let legacy = rmp_serde::to_vec_named(&refs).expect("serialize legacy blob");
    std::fs::write(store.blob_path_rref_hex(&hash_hex), legacy).expect("write legacy blob");

    assert_eq!(store.read_resolved_by_hex(&hash_hex).unwrap(), Some(refs));
}

#[test]
fn writing_a_current_single_map_repairs_a_corrupt_compressed_payload() {
    use crate::intel::model::FileResolvedRefs;

    init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
    let hash_hex = "6".repeat(64);
    let refs = FileResolvedRefs::new("rust");
    store.write_resolved_hex(&hash_hex, &refs).expect("write resolved blob");

    let path = store.blob_path_rref_hex(&hash_hex);
    let mut persisted = std::fs::read(&path).expect("read resolved bytes");
    persisted[SINGLE_HEADER_LEN] ^= 0xff;
    std::fs::write(&path, persisted).expect("corrupt compressed payload");

    store.write_resolved_hex(&hash_hex, &refs).expect("repair corrupt blob");
    assert_eq!(store.read_resolved_by_hex(&hash_hex).unwrap(), Some(refs));
}

#[cfg(feature = "code-search")]
#[test]
fn legacy_uncompressed_chunk_blobs_remain_readable_and_peekable() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
    let hash_hex = "4".repeat(64);
    let blob = crate::chunk::CodeChunkBlob {
        config_digest: String::new(),
        schema_ver: SCHEMA_VER,
        extract_epoch: 0,
        embedding_dim: 768,
        embedding_model: "balanced".to_string(),
        chunks: Vec::new(),
        embeddings: Vec::new(),
    };
    let legacy = rmp_serde::to_vec_named(&blob).expect("serialize legacy chunk blob");
    std::fs::write(store.blob_path_chunk_hex(&hash_hex), legacy).expect("write legacy chunk blob");

    assert_eq!(store.read_chunks_by_hex(&hash_hex).unwrap(), Some(blob));
    let peek = store.peek_chunk_state(&hash_hex).unwrap().expect("legacy chunk peek");
    assert_eq!(peek.extract_epoch, 0);
    assert_eq!(peek.embedding_dim, 768);
    assert_eq!(peek.embedding_model, "balanced");
    assert_eq!(peek.chunks.len(), 0);
    assert_eq!(peek.embeddings.len(), 0);
}

#[cfg(feature = "code-search")]
#[test]
fn chunk_peek_reads_plain_metadata_without_decompressing_payload() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
    let hash_hex = "3".repeat(64);
    let blob = crate::chunk::CodeChunkBlob {
        config_digest: String::new(),
        schema_ver: SCHEMA_VER,
        extract_epoch: crate::extract::EXTRACT_EPOCH,
        embedding_dim: 768,
        embedding_model: "balanced".to_string(),
        chunks: Vec::new(),
        embeddings: Vec::new(),
    };
    store.write_chunks_hex(&hash_hex, &blob).expect("write chunk blob");

    let path = store.blob_path_chunk_hex(&hash_hex);
    let mut persisted = std::fs::read(&path).expect("read chunk blob bytes");
    *persisted.last_mut().expect("compressed payload present") ^= 0xff;
    std::fs::write(&path, persisted).expect("corrupt compressed chunk payload");

    let peek = store
        .peek_chunk_state(&hash_hex)
        .expect("plain chunk metadata remains readable")
        .expect("chunk peek present");
    assert_eq!(
        peek.extract_epoch,
        crate::extract::EXTRACT_EPOCH,
        "the epoch is in the plain header"
    );
    assert_eq!(peek.embedding_dim, 768);
    assert_eq!(peek.embedding_model, "balanced");
    assert_eq!(peek.chunks.len(), 0);
    assert_eq!(peek.embeddings.len(), 0);
    assert!(
        store.read_chunks_by_hex(&hash_hex).is_err(),
        "full read must observe corruption"
    );
}

/// A chunk whose text is pseudo-random (so zstd cannot shrink it away), to force a large payload.
#[cfg(feature = "code-search")]
fn noisy_chunk(i: u32) -> crate::chunk::CodeChunk {
    let mut x = i.wrapping_mul(2_654_435_761).wrapping_add(1);
    let text: String = (0..64)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            char::from(b'a' + (x % 26) as u8)
        })
        .collect();
    crate::chunk::CodeChunk {
        chunk_id: format!("h:{i}"),
        path: "a.rs".to_string(),
        lang: "rust".to_string(),
        kind: None,
        symbol: None,
        signature: None,
        doc: None,
        byte_start: 0,
        byte_end: 64,
        line_start: 1,
        line_end: 1,
        searchable_text: text.clone(),
        text,
    }
}

#[cfg(feature = "code-search")]
#[test]
fn chunk_peek_prefix_read_recovers_digest_and_counts_with_large_payload() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
    let hash_hex = "5".repeat(64);
    let digest = "d".repeat(crate::config::rules::DIGEST_LEN);
    let blob = crate::chunk::CodeChunkBlob {
        config_digest: digest.clone(),
        schema_ver: SCHEMA_VER,
        extract_epoch: crate::extract::EXTRACT_EPOCH,
        embedding_dim: 4,
        embedding_model: "balanced".to_string(),
        chunks: (0..200).map(noisy_chunk).collect(),
        embeddings: (0..200).map(|i| vec![i as f32; 4]).collect(),
    };
    store.write_chunks_hex(&hash_hex, &blob).expect("write chunk blob");
    assert!(
        std::fs::metadata(store.blob_path_chunk_hex(&hash_hex)).unwrap().len() > 2_000,
        "payload must dwarf the header so a prefix read is distinguishable from a whole read"
    );

    let peek = store.peek_chunk_state(&hash_hex).unwrap().expect("peek present");
    assert_eq!(peek.config_digest, digest);
    assert_eq!(peek.extract_epoch, crate::extract::EXTRACT_EPOCH);
    assert_eq!(peek.chunks.len(), 200);
    assert_eq!(peek.embeddings.len(), 200);
    assert_eq!(store.read_chunks_by_hex(&hash_hex).unwrap(), Some(blob));
}

/// A chunk envelope as written before the header carried the extract epoch: kind `Chunk`, a 28-byte
/// fixed header, and a payload without the field.
#[cfg(feature = "code-search")]
fn pre_epoch_chunk_envelope(blob: &crate::chunk::CodeChunkBlob) -> Vec<u8> {
    let bytes = rmp_serde::to_vec_named(blob).unwrap();
    let compressed = compress_payload(&bytes).unwrap();
    let model = blob.embedding_model.as_bytes();
    let mut out = Vec::new();
    encode_prefix(BlobKind::Chunk, blob.schema_ver, &mut out);
    out.extend_from_slice(&checked_u32_len(bytes.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&checked_u32_len(compressed.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&blob.embedding_dim.to_le_bytes());
    out.extend_from_slice(&u16::try_from(model.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&checked_u32_len(blob.chunks.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&checked_u32_len(blob.embeddings.len()).unwrap().to_le_bytes());
    out.extend_from_slice(model);
    out.extend_from_slice(chunk_digest_region(&blob.config_digest));
    out.extend_from_slice(&compressed);
    out
}

/// A sidecar written before the chunk header carried the extract epoch must read as epoch 0, so the
/// scanner re-chunks it, and must not be an error, which would drop it as damaged.
#[cfg(feature = "code-search")]
#[test]
fn a_chunk_envelope_without_an_epoch_reads_as_epoch_zero() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
    let hash_hex = "7".repeat(64);
    let blob = crate::chunk::CodeChunkBlob {
        config_digest: "d".repeat(crate::config::rules::DIGEST_LEN),
        schema_ver: SCHEMA_VER,
        extract_epoch: 0,
        embedding_dim: 4,
        embedding_model: "balanced".to_string(),
        chunks: (0..3).map(noisy_chunk).collect(),
        embeddings: (0..3).map(|i| vec![i as f32; 4]).collect(),
    };
    std::fs::write(store.blob_path_chunk_hex(&hash_hex), pre_epoch_chunk_envelope(&blob)).unwrap();

    let peek = store.peek_chunk_state(&hash_hex).unwrap().expect("peek present");
    assert_eq!(peek.extract_epoch, 0, "no epoch in the header reads as stale");
    assert_eq!(peek.config_digest, blob.config_digest);
    assert_eq!(peek.chunks.len(), 3);
    assert_eq!(store.read_chunks_by_hex(&hash_hex).unwrap(), Some(blob));
}

/// The writer stamps the epoch, and a stamped sidecar round-trips with it in the header and payload.
#[cfg(feature = "code-search")]
#[test]
fn a_written_chunk_sidecar_carries_its_extract_epoch() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
    let hash_hex = "8".repeat(64);
    let blob = crate::chunk::CodeChunkBlob {
        config_digest: String::new(),
        schema_ver: SCHEMA_VER,
        extract_epoch: crate::extract::EXTRACT_EPOCH,
        embedding_dim: 0,
        embedding_model: String::new(),
        chunks: (0..2).map(noisy_chunk).collect(),
        embeddings: Vec::new(),
    };
    store.write_chunks_hex(&hash_hex, &blob).expect("write chunk blob");

    let peek = store.peek_chunk_state(&hash_hex).unwrap().expect("peek present");
    assert_eq!(peek.extract_epoch, crate::extract::EXTRACT_EPOCH);
    assert_eq!(peek.chunks.len(), 2);
    assert_eq!(store.read_chunks_by_hex(&hash_hex).unwrap(), Some(blob));
}
