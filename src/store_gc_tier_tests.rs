//! Tier-aware liveness: a blob suffix is kept alive only by the tier that produces it (see
//! [`super::LiveBlobs`]).

use std::fs;
use std::time::Duration;

use super::tests::{Fixture, build_fixture};
use super::*;
use crate::store::{FileEntry, INDEX_FILE, Index, VIEWS_DIR};
use crate::store_cache_admin::cache_stats_in;

/// Hand-write the working view's index with one code entry and one document entry.
pub(super) fn write_index_with(fx: &Fixture, code_hash: Option<&str>, doc_hash: Option<&str>) {
    let mut index = Index::empty();
    if let Some(hash) = code_hash {
        index.files.insert(
            crate::path::RelPath::from("src/lib.rs"),
            FileEntry {
                hash_hex: hash.to_string(),
                language: "rust".to_string(),
                size_bytes: 2,
                mtime: 0,
                extract_epoch: crate::extract::EXTRACT_EPOCH,
            },
        );
    }
    if let Some(hash) = doc_hash {
        index.doc_files.insert(
            crate::path::RelPath::from("README.md"),
            crate::store::DocEntry {
                hash_hex: hash.to_string(),
                embedding_preset: "balanced".to_string(),
                size_bytes: 2,
                mtime: 0,
                embedded: true,
                embed_attempted: true,
                config_digest: String::new(),
            },
        );
    }
    let bytes = rmp_serde::to_vec_named(&index).expect("encode index");
    fs::write(fx.basemind_dir.join(VIEWS_DIR).join("working").join(INDEX_FILE), bytes).expect("write index");
}

fn write_all_suffixes(fx: &Fixture, stem: &str) {
    for suffix in [".fm.msgpack", ".chunk.msgpack", ".rref.msgpack", ".doc.msgpack"] {
        fs::write(fx.blobs_dir.join(format!("{stem}{suffix}")), b"x").expect("write blob");
    }
}

fn present(fx: &Fixture, stem: &str) -> Vec<&'static str> {
    [".fm.msgpack", ".chunk.msgpack", ".rref.msgpack", ".doc.msgpack"]
        .into_iter()
        .filter(|suffix| fx.blobs_dir.join(format!("{stem}{suffix}")).exists())
        .collect()
}

/// A path that moved from the code map to the document tier keeps its content hash, so its stale
/// `.fm` / `.chunk` / `.rref` blobs share a stem with the live `.doc` blob. Only the tier that
/// references a suffix may keep it alive.
#[test]
fn document_reference_reclaims_the_code_lane_blobs_that_share_its_stem() {
    let fx = build_fixture();
    let stem = "c".repeat(64);
    write_all_suffixes(&fx, &stem);
    write_index_with(&fx, None, Some(&stem));

    let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
    let report = gc_blobs_in(&fx.blobs_dir, &referenced, Duration::ZERO).expect("gc");

    assert_eq!(
        present(&fx, &stem),
        vec![".doc.msgpack"],
        "only the document-lane blob survives"
    );
    assert_eq!(
        report.removed, 5,
        "the 3 stale code-lane blobs plus the 2 fixture blobs nothing references now"
    );
}

/// The mirror image: a path that left the document tier for the code map keeps its code-lane
/// blobs and loses the stale `.doc` blob.
#[test]
fn code_reference_reclaims_the_stale_document_blob_that_shares_its_stem() {
    let fx = build_fixture();
    let stem = "d".repeat(64);
    write_all_suffixes(&fx, &stem);
    write_index_with(&fx, Some(&stem), None);

    let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
    gc_blobs_in(&fx.blobs_dir, &referenced, Duration::ZERO).expect("gc");

    assert_eq!(
        present(&fx, &stem),
        vec![".fm.msgpack", ".chunk.msgpack", ".rref.msgpack"]
    );
}

/// A hash referenced by BOTH tiers (two paths with identical bytes, one of each kind) keeps every
/// suffix.
#[test]
fn a_hash_referenced_by_both_tiers_keeps_every_blob() {
    let fx = build_fixture();
    let stem = "e".repeat(64);
    write_all_suffixes(&fx, &stem);
    write_index_with(&fx, Some(&stem), Some(&stem));

    let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
    gc_blobs_in(&fx.blobs_dir, &referenced, Duration::ZERO).expect("gc");

    assert_eq!(present(&fx, &stem).len(), 4);
}

#[test]
fn cache_stats_counts_cross_tier_orphans() {
    let fx = build_fixture();
    let stem = "f".repeat(64);
    write_all_suffixes(&fx, &stem);
    write_index_with(&fx, None, Some(&stem));

    let stats = cache_stats_in(&fx.basemind_dir, &fx.blobs_dir).expect("cache_stats");

    // 3 stale code-lane blobs + the fixture's unreferenced `b..` orphan + its now-unreferenced `a..` fm.
    assert_eq!(stats.orphan_blob_count, 5);
}
