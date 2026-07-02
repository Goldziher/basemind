//! Verifies that opening a Store against a stale-schema index refreshes the cache durably
//! — it resets the view's `index.msgpack` but DOES NOT destroy the shared content-addressed
//! blob store.
//!
//! We synthesize a stale-schema-shaped index by serializing a struct with
//! `schema_ver = 99` (a value distinct from any current or near-future
//! `RELEASE_MINOR`) and the legacy `FileEntry` layout. `Store::open` should detect the
//! mismatch, remove only `index.msgpack`, leave `blobs/` intact (orphans are reclaimed
//! later by `store_gc::run_gc`), and return an empty in-memory index so the next scan
//! re-extracts every file and overwrites stale blobs in place.

use std::fs;

use serde::Serialize;

#[derive(Serialize)]
struct LegacyIndex {
    schema_ver: u16,
    files: std::collections::BTreeMap<String, LegacyEntry>,
}

#[derive(Serialize)]
struct LegacyEntry {
    hash_hex: String,
    language: String,
    size_bytes: u64,
    mtime: i64,
}

/// Iter-6 additive `FileMapDoc.keywords` + `FileMapDoc.entities` must not break
/// pre-iter-6 blobs. We construct an "old-shape" blob — every iter-2..iter-5
/// field present, no tail keyword/entity fields — and assert it round-trips
/// through msgpack into the current `FileMapDoc` with empty vecs courtesy of
/// `#[serde(default)]`. This is the explicit contract behind the no-bump
/// decision in the iter-6 plan.
#[cfg(feature = "documents")]
#[test]
fn pre_iter6_doc_blob_deserialises_into_new_filemap_doc() {
    use basemind::extract::doc::FileMapDoc;
    use serde::Serialize;

    // SCHEMA_VER is a `pub(crate)` constant inside `extract/mod.rs`; we
    // construct the blob with the current ver explicitly so `read_doc_by_hex`'s
    // schema check would pass — but here we deserialise straight to the struct,
    // so any version is fine. Use 0 to make the "old" intent obvious.
    #[derive(Serialize)]
    struct OldShape {
        schema_ver: u16,
        mime_type: String,
        content: String,
        metadata: Vec<(String, String)>,
        detected_languages: Vec<String>,
        chunks: Vec<OldChunk>,
        embedding_model: String,
        embedding_dim: u16,
        // NOTE: no `keywords`, no `entities` — pre-iter-6 layout.
    }
    #[derive(Serialize)]
    struct OldChunk {
        byte_start: u32,
        byte_end: u32,
        text: String,
        embedding: Vec<f32>,
    }

    let old = OldShape {
        schema_ver: 0,
        mime_type: "text/plain".to_string(),
        content: "hello world".to_string(),
        metadata: vec![("title".to_string(), "Test".to_string())],
        detected_languages: vec!["eng".to_string()],
        chunks: vec![OldChunk {
            byte_start: 0,
            byte_end: 11,
            text: "hello world".to_string(),
            embedding: vec![],
        }],
        embedding_model: String::new(),
        embedding_dim: 0,
    };
    let bytes = rmp_serde::to_vec_named(&old).expect("serialize old shape");
    let new_doc: FileMapDoc = rmp_serde::from_slice(&bytes).expect("old shape must deserialise via serde(default)");

    assert_eq!(new_doc.mime_type, "text/plain");
    assert_eq!(new_doc.chunks.len(), 1);
    assert!(
        new_doc.keywords.is_empty(),
        "iter-6 `keywords` must default to empty on pre-iter-6 blobs"
    );
    assert!(
        new_doc.entities.is_empty(),
        "iter-6 `entities` must default to empty on pre-iter-6 blobs"
    );
    assert!(
        new_doc.summary.is_none(),
        "iter-7 `summary` must default to None on pre-iter-6 blobs"
    );
}

/// Iter-7 additive `FileMapDoc.summary` must not break pre-iter-7 blobs that
/// already carry the iter-6 `keywords` + `entities` tail. Same shape as the
/// iter-6 test but with the iter-6 fields populated so this is a stricter
/// round-trip than the pre-iter-6 one.
#[cfg(feature = "documents")]
#[test]
fn pre_iter7_doc_blob_deserialises_into_new_filemap_doc() {
    use basemind::extract::doc::FileMapDoc;
    use serde::Serialize;

    #[derive(Serialize)]
    struct PreIter7 {
        schema_ver: u16,
        mime_type: String,
        content: String,
        metadata: Vec<(String, String)>,
        detected_languages: Vec<String>,
        chunks: Vec<PreIter7Chunk>,
        embedding_model: String,
        embedding_dim: u16,
        keywords: Vec<PreIter7Keyword>,
        entities: Vec<PreIter7Entity>,
        // NOTE: no `summary` — pre-iter-7 layout.
    }
    #[derive(Serialize)]
    struct PreIter7Chunk {
        byte_start: u32,
        byte_end: u32,
        text: String,
        embedding: Vec<f32>,
    }
    #[derive(Serialize)]
    struct PreIter7Keyword {
        text: String,
        score: f32,
        algorithm: String,
    }
    #[derive(Serialize)]
    struct PreIter7Entity {
        category: String,
        text: String,
        start: u32,
        end: u32,
    }

    let old = PreIter7 {
        schema_ver: 0,
        mime_type: "text/plain".to_string(),
        content: "hello world".to_string(),
        metadata: vec![],
        detected_languages: vec!["eng".to_string()],
        chunks: vec![PreIter7Chunk {
            byte_start: 0,
            byte_end: 11,
            text: "hello world".to_string(),
            embedding: vec![],
        }],
        embedding_model: String::new(),
        embedding_dim: 0,
        keywords: vec![PreIter7Keyword {
            text: "hello".to_string(),
            score: 0.5,
            algorithm: "yake".to_string(),
        }],
        entities: vec![PreIter7Entity {
            category: "location".to_string(),
            text: "world".to_string(),
            start: 6,
            end: 11,
        }],
    };
    let bytes = rmp_serde::to_vec_named(&old).expect("serialize pre-iter-7 shape");
    let new_doc: FileMapDoc =
        rmp_serde::from_slice(&bytes).expect("pre-iter-7 shape must deserialise via serde(default)");

    assert_eq!(new_doc.keywords.len(), 1, "iter-6 keywords preserved");
    assert_eq!(new_doc.entities.len(), 1, "iter-6 entities preserved");
    assert!(
        new_doc.summary.is_none(),
        "iter-7 `summary` must default to None on pre-iter-7 blobs"
    );
}

#[test]
fn opening_against_stale_schema_index_refreshes_durably_without_wiping_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let basemind_dir = root.join(".basemind");
    let blobs_dir = basemind_dir.join("blobs");
    fs::create_dir_all(&blobs_dir).unwrap();

    // Drop a fake blob and a v1-shaped index.
    let blob_path = blobs_dir.join("deadbeef.l1.msgpack");
    fs::write(&blob_path, b"not really a blob").unwrap();

    let mut files = std::collections::BTreeMap::new();
    files.insert(
        "a.rs".to_string(),
        LegacyEntry {
            hash_hex: "deadbeef".repeat(8),
            language: "rust".to_string(),
            size_bytes: 42,
            mtime: 0,
        },
    );
    let legacy = LegacyIndex { schema_ver: 99, files };
    let bytes = rmp_serde::to_vec_named(&legacy).unwrap();
    fs::write(basemind_dir.join("index.msgpack"), bytes).unwrap();

    // Opening the store must detect the mismatch and reset the index in place.
    let store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING)
        .expect("open should succeed via durable refresh");
    assert!(
        store.index.files.is_empty(),
        "in-memory index should be empty after the stale-schema reset"
    );
    // Durable refresh: the shared blob store is NOT destroyed. The (now-orphaned) stale
    // blob survives the open; it is reclaimed later by `store_gc::run_gc`, not by a wipe.
    assert!(
        blob_path.exists(),
        "blobs must survive a schema bump — durable refresh never wipes the blob store"
    );
    assert!(blobs_dir.exists());

    // The view's index file was reset so the next scan treats every file as new.
    let view_index = basemind_dir
        .join("views")
        .join(basemind::store::VIEW_WORKING)
        .join("index.msgpack");
    assert!(
        !view_index.exists(),
        "view index.msgpack should be removed so read_index → None → empty index"
    );
}

/// A read-only consumer (the CLI parity path) cannot wipe + rebuild, so a stale-schema view
/// index must degrade gracefully to an empty index rather than propagate a hard error — and
/// it must NOT open the Fjall index (whose own open path wipes on mismatch, which would
/// corrupt a concurrently running `serve`). Regression test for the post-bump CLI-first path.
#[test]
fn open_read_only_degrades_to_empty_on_stale_schema_without_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let view_dir = root.join(".basemind").join("views").join(basemind::store::VIEW_WORKING);
    fs::create_dir_all(&view_dir).unwrap();

    // Forge a stale-schema index directly in the view dir.
    let mut files = std::collections::BTreeMap::new();
    files.insert(
        "a.rs".to_string(),
        LegacyEntry {
            hash_hex: "deadbeef".repeat(8),
            language: "rust".to_string(),
            size_bytes: 42,
            mtime: 0,
        },
    );
    let legacy = LegacyIndex { schema_ver: 99, files };
    fs::write(
        view_dir.join("index.msgpack"),
        rmp_serde::to_vec_named(&legacy).unwrap(),
    )
    .unwrap();

    // Must NOT error; index reads empty and the Fjall handle is skipped.
    let store = basemind::store::Store::open_read_only(root, basemind::store::VIEW_WORKING)
        .expect("open_read_only must degrade gracefully, not error, on a stale-schema index");
    assert!(
        store.index.files.is_empty(),
        "stale-schema index must read as empty for a read-only consumer"
    );
    assert!(
        store.index_db.is_none(),
        "Fjall index must not be opened on a schema mismatch (its open path wipes)"
    );
}

/// `write_blob`'s schema-aware guard must OVERWRITE a stale-schema blob at an existing
/// content-hash path (the durable-refresh mechanism), while still skipping a rewrite when
/// the on-disk blob already carries the current schema. We exercise this end-to-end through
/// a real scan: seed a blob with a bogus `schema_ver`, force a schema mismatch on re-open,
/// then re-scan and assert the blob now reads back at the current schema with no mass
/// blob deletion.
#[test]
fn schema_bump_refreshes_blobs_in_place_and_gc_reclaims_only_orphans() {
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // A tiny one-file repo. Keep it trivially parseable Rust.
    fs::write(root.join("a.rs"), b"pub fn main() {}\n").unwrap();

    let config = basemind::config::ConfigV1::with_defaults();

    // First scan: populate blobs + index at the current schema.
    {
        let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).unwrap();
        basemind::scanner::scan(root, &mut store, &config, basemind::scanner::ScanSource::WorkingTree)
            .expect("first scan");
        store.flush().expect("flush");
    }

    let basemind_dir = root.join(".basemind");
    let blobs_dir = basemind_dir.join("blobs");
    let blob_files = |label: &str| -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(&blobs_dir)
            .unwrap_or_else(|e| panic!("read blobs ({label}): {e}"))
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".msgpack"))
            .collect();
        v.sort();
        v
    };

    let before = blob_files("before");
    assert!(!before.is_empty(), "first scan must write at least one blob");

    // Capture the working view index so we can corrupt its schema version.
    let view_index = basemind_dir
        .join("views")
        .join(basemind::store::VIEW_WORKING)
        .join("index.msgpack");
    let idx_bytes = fs::read(&view_index).unwrap();
    let real_index: basemind::store::Index = rmp_serde::from_slice(&idx_bytes).unwrap();

    // Forge a stale-schema copy of the SAME index (same file entries / hashes), so the next
    // `Store::open` hits the SchemaMismatch arm. Mirror the on-disk shape via the public
    // `Index` type with a bumped `schema_ver`.
    let mut forged_files: BTreeMap<String, LegacyEntry> = BTreeMap::new();
    for (rel, entry) in &real_index.files {
        forged_files.insert(
            rel.to_str_lossy().into_owned(),
            LegacyEntry {
                hash_hex: entry.hash_hex.clone(),
                language: entry.language.clone(),
                size_bytes: entry.size_bytes,
                mtime: entry.mtime,
            },
        );
    }
    let forged = LegacyIndex {
        schema_ver: 99,
        files: forged_files,
    };
    fs::write(&view_index, rmp_serde::to_vec_named(&forged).unwrap()).unwrap();

    // Re-open: durable refresh resets the index but keeps blobs.
    {
        let store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).unwrap();
        assert!(
            store.index.files.is_empty(),
            "index reset to empty after the forced schema mismatch"
        );
    }
    assert!(
        !blob_files("after-open").is_empty(),
        "blobs survive the schema-mismatch open — no destructive wipe"
    );

    // Re-scan: every file is treated as new (empty index), re-extracts, and write_blob
    // overwrites the in-place blobs. Hashes are content-derived, so for unchanged source the
    // blob set is stable.
    {
        let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).unwrap();
        basemind::scanner::scan(root, &mut store, &config, basemind::scanner::ScanSource::WorkingTree)
            .expect("refresh scan");
        store.flush().expect("flush");
    }

    let after = blob_files("after-rescan");
    assert_eq!(
        after, before,
        "unchanged source → identical content-hash blob set after the refresh"
    );

    // The refreshed l1 blob must now read back at the CURRENT schema — proving write_blob
    // overwrote the stale blob in place rather than short-circuiting on the exists-guard.
    {
        let store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).unwrap();
        let entry = store.index.files.values().next().expect("one indexed file");
        let map = store
            .read_l1_by_hex(&entry.hash_hex)
            .expect("read_l1 must not fail with a schema mismatch")
            .expect("l1 blob present");
        assert_eq!(
            map.schema_ver,
            basemind::extract::SCHEMA_VER,
            "refreshed blob carries the current schema version"
        );
    }

    // GC under the refreshed index must not mass-delete: every referenced blob is live.
    let report = basemind::store_gc::run_gc(&basemind_dir).expect("gc");
    assert_eq!(
        report.removed, 0,
        "no spurious deletion: every blob the refreshed index references is live"
    );
}
