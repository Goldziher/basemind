//! Document-tier memory bounds, end to end through `scan`.
//!
//! A scan over a document-heavy repository grew to 33 GB resident. These tests pin the two
//! pre-flight guards that do not need a model download: a document whose estimated working set
//! cannot fit under `[resources] max_footprint_mb` is skipped with a reason instead of being
//! extracted, and an archive past the configured uncompressed cap fails instead of being unpacked.

#![cfg(feature = "documents")]

use std::fs;

use basemind::config::{ConfigV1, MaxFootprint};
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::store::Store;

fn scan_once(root: &std::path::Path, cfg: &ConfigV1) -> basemind::scanner::ScanReport {
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
    scan(root, &mut store, cfg, ScanSource::WorkingTree, EmbedMode::Inline).expect("scan")
}

fn offline_config() -> ConfigV1 {
    basemind::store::init_isolated_cache();
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.embed = false;
    cfg
}

/// A PNG whose 33-byte header claims 30,000 x 30,000 pixels (~57 GB at the estimator's per-pixel
/// cost) while the file itself is a few dozen bytes: on-disk size says nothing about memory.
fn png_claiming_huge_dimensions() -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    png.extend_from_slice(&30_000u32.to_be_bytes());
    png.extend_from_slice(&30_000u32.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);
    png
}

#[test]
fn a_document_that_cannot_fit_under_the_ceiling_is_skipped_not_extracted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = offline_config();
    cfg.resources.max_footprint_mb = MaxFootprint::Mebibytes(512);
    fs::write(dir.path().join("huge.png"), png_claiming_huge_dimensions()).unwrap();
    fs::write(
        dir.path().join("small.svg"),
        br#"<svg xmlns="http://www.w3.org/2000/svg"><text>the quick brown fox jumps</text></svg>"#,
    )
    .unwrap();

    let report = scan_once(dir.path(), &cfg);

    assert_eq!(
        report.stats.skipped_too_large, 1,
        "the over-budget image must be skipped, not sent to xberg"
    );
    assert_eq!(
        report.stats.docs_indexed, 1,
        "the small document in the same scan must still be indexed (the guard is per-document)"
    );
    assert_eq!(
        report.stats.extract_failed, 0,
        "a skip is a verdict on the file, not an extraction failure"
    );
    let store = Store::open(dir.path(), basemind::store::VIEW_WORKING).unwrap();
    assert!(store.lookup_doc("huge.png").is_none());
    assert!(store.lookup_doc("small.svg").is_some());
}

#[test]
fn the_same_image_is_not_pre_skipped_when_the_ceiling_is_off() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = offline_config();
    cfg.resources.max_footprint_mb = MaxFootprint::Keyword(basemind::config::FootprintKeyword::Off);
    fs::write(dir.path().join("huge.png"), png_claiming_huge_dimensions()).unwrap();

    let report = scan_once(dir.path(), &cfg);

    // The truncated PNG is not decodable, so xberg rejects it: the point is that it REACHED xberg,
    // i.e. the skip above is caused by the ceiling and nothing else.
    assert_eq!(report.stats.skipped_too_large, 0);
    assert_eq!(
        report.stats.docs_indexed + report.stats.extract_failed + report.stats.skipped_no_lang,
        1
    );
}

/// CRC-32 (IEEE), bitwise. Only for building a valid stored zip entry in the test below.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A valid single-entry stored (uncompressed) zip.
fn stored_zip(name: &str, data: &[u8]) -> Vec<u8> {
    let crc = crc32(data);
    let size = data.len() as u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"PK\x03\x04");
    out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(data);
    let cd_offset = out.len() as u32;
    out.extend_from_slice(b"PK\x01\x02");
    out.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&[0; 12]);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    let cd_size = out.len() as u32 - cd_offset;
    out.extend_from_slice(b"PK\x05\x06\x00\x00\x00\x00\x01\x00\x01\x00");
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[test]
fn archive_extraction_honours_the_configured_uncompressed_cap() {
    let text = "the quick brown fox jumps over the lazy dog\n".repeat(200);
    assert!(text.len() > 4096);
    let archive = stored_zip("notes.txt", text.as_bytes());

    // Generous cap: the entry is unpacked and indexed.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = offline_config();
    cfg.documents.extract_archives = true;
    fs::write(dir.path().join("bundle.zip"), &archive).unwrap();
    let open = scan_once(dir.path(), &cfg);
    assert_eq!(
        open.stats.docs_indexed, 1,
        "control: with the default cap this archive extracts (stats: {:?})",
        open.stats
    );

    // Cap below the entry's size: nothing may be unpacked. A different payload, so the content-
    // addressed extraction cache can't answer for the first scan's identical archive.
    let text = format!("capped run\n{text}");
    let archive = stored_zip("notes.txt", text.as_bytes());
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = offline_config();
    cfg.documents.extract_archives = true;
    cfg.documents.max_archive_bytes = 1024;
    fs::write(dir.path().join("bundle.zip"), &archive).unwrap();
    let capped = scan_once(dir.path(), &cfg);
    assert_eq!(
        capped.stats.docs_indexed, 0,
        "an archive past max_archive_bytes must not be unpacked (stats: {:?})",
        capped.stats
    );
}
