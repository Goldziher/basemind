//! What a `documents` build reports for work that needs a tier it was not compiled with
//! (`documents-ocr`, `documents-ner`), and for the capabilities that were removed outright.
//! Nothing here downloads a model.

#![cfg(feature = "documents")]

use std::fs;
use std::path::Path;

use basemind::config::ConfigV1;
use basemind::scanner::{CollectObserver, EmbedMode, FileStatus, ScanCancel, ScanSource, scan_with_observer};
use basemind::store::Store;

fn offline_config() -> ConfigV1 {
    basemind::store::init_isolated_cache();
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.embed = false;
    // `*.svg` is a default `[documents] exclude`; clearing the list opts the fixtures back in.
    cfg.documents.exclude = Vec::new();
    cfg
}

fn scan_collecting(root: &Path, cfg: &ConfigV1) -> (basemind::scanner::ScanReport, CollectObserver) {
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
    let mut observer = CollectObserver::new();
    let report = scan_with_observer(
        root,
        &mut store,
        cfg,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        &ScanCancel::new(),
        &mut observer,
    )
    .expect("scan");
    (report, observer)
}

const SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg"><text>the quick brown fox jumps</text></svg>"#;

/// A valid single-entry stored (uncompressed) zip holding `notes.txt`.
fn stored_zip() -> Vec<u8> {
    let name = b"notes.txt";
    let data = b"the quick brown fox jumps over the lazy dog\n";
    let crc: u32 = {
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
    };
    let size = data.len() as u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"PK\x03\x04");
    out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(name);
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
    out.extend_from_slice(name);
    let cd_size = out.len() as u32 - cd_offset;
    out.extend_from_slice(b"PK\x05\x06\x00\x00\x00\x00\x01\x00\x01\x00");
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[test]
fn an_archive_is_skipped_even_when_extract_archives_is_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = offline_config();
    cfg.documents.extract_archives = true;
    fs::write(dir.path().join("bundle.zip"), stored_zip()).unwrap();
    fs::write(dir.path().join("small.svg"), SVG).unwrap();

    let (report, observer) = scan_collecting(dir.path(), &cfg);

    assert_eq!(
        report.stats.docs_indexed, 1,
        "only the svg is a document (stats: {:?})",
        report.stats
    );
    assert_eq!(report.stats.extract_failed, 0, "a skip, not a failed extraction");
    assert!(
        observer
            .results()
            .iter()
            .all(|result| !matches!(result.status, FileStatus::DocIndexed { .. }) || result.path == "small.svg"),
        "the archive must not reach the extractor"
    );
    let store = Store::open(dir.path(), basemind::store::VIEW_WORKING).unwrap();
    assert!(store.lookup_doc("bundle.zip").is_none());
    assert!(store.lookup_doc("small.svg").is_some());
}

#[test]
fn svg_text_is_extracted_through_the_xml_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("diagram.svg");
    fs::write(&path, SVG).unwrap();
    let config = basemind::extract::doc::DocConfig {
        embed: false,
        ..Default::default()
    };

    let doc = basemind::extract::doc::extract_doc(&path, Some("image/svg+xml"), &config).expect("extract svg");

    assert!(doc.content.contains("the quick brown fox jumps"), "{:?}", doc.content);
}

#[cfg(not(feature = "documents-ocr"))]
mod without_ocr {
    use super::*;

    /// A PNG signature and a 1x1 header. Routing is by MIME type, which comes from the extension.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00";

    /// A one-page PDF with no content stream: structurally valid, no text layer.
    fn textless_pdf() -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>",
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn an_image_is_not_indexed_as_a_document() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = offline_config();
        fs::write(dir.path().join("photo.png"), PNG).unwrap();
        fs::write(dir.path().join("small.svg"), SVG).unwrap();

        let (report, _) = scan_collecting(dir.path(), &cfg);

        assert_eq!(
            report.stats.docs_indexed, 1,
            "the svg is text, the png needs OCR (stats: {:?})",
            report.stats
        );
        assert_eq!(report.stats.extract_failed, 0);
        let store = Store::open(dir.path(), basemind::store::VIEW_WORKING).unwrap();
        assert!(
            store.lookup_doc("photo.png").is_none(),
            "an image must not become a metadata-only document"
        );
        assert!(store.lookup_doc("small.svg").is_some());
    }

    #[test]
    fn a_pdf_without_a_text_layer_is_a_named_failure_not_an_empty_document() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = offline_config();
        fs::write(dir.path().join("scan.pdf"), textless_pdf()).unwrap();

        let (report, observer) = scan_collecting(dir.path(), &cfg);

        assert_eq!(report.stats.docs_indexed, 0, "stats: {:?}", report.stats);
        assert_eq!(report.stats.extract_failed, 1, "stats: {:?}", report.stats);
        let failure = observer
            .results()
            .iter()
            .find_map(|result| match &result.status {
                FileStatus::ExtractFailed { msg } if result.path == "scan.pdf" => Some(msg.clone()),
                _ => None,
            })
            .expect("scan.pdf is reported as a failed extraction");
        assert!(failure.contains("no text extracted (OCR not compiled in)"), "{failure}");
        assert!(failure.contains("documents-ocr"), "{failure}");
        let store = Store::open(dir.path(), basemind::store::VIEW_WORKING).unwrap();
        assert!(store.lookup_doc("scan.pdf").is_none());
    }
}

#[cfg(not(feature = "documents-ner"))]
mod without_ner {
    use super::*;
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use serde_json::json;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_entity_filter_is_refused_naming_the_feature() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let cfg = offline_config();
        fs::write(root.join("small.svg"), SVG).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| scan_collecting(&root, &cfg));
        });

        let transport = basemind::mcp::serve_in_memory(&root, "working")
            .await
            .expect("in-memory serve");
        let service = ().serve(transport).await.expect("rmcp handshake");
        let arguments = json!({ "mode": "documents", "query": "fox", "entity_category": "person" });
        let params = CallToolRequestParams::new("memory").with_arguments(arguments.as_object().unwrap().clone());

        let error = service
            .call_tool(params)
            .await
            .expect_err("an entity filter cannot be answered without the NER tier");

        let message = format!("{error:?}");
        assert!(message.contains("entity_category"), "{message}");
        assert!(message.contains("documents-ner"), "{message}");
        let _ = service.cancel().await;
    }
}
