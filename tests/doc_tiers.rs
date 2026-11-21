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
        basemind::retrieval_wiring::backend(),
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

/// One file per document format a tree-sitter grammar also claims, by its own extension.
const MARKUP: &[(&str, &str)] = &[
    (
        "page.html",
        "<html><body><h1>Pumps</h1><p>The quick brown fox.</p></body></html>",
    ),
    ("old.htm", "<html><body><p>The quick brown fox.</p></body></html>"),
    (
        "paper.tex",
        "\\documentclass{article}\n\\begin{document}\nThe quick brown fox.\n\\end{document}\n",
    ),
    ("notes.org", "* Pumps\nThe quick brown fox.\n"),
    ("report.typ", "= Pumps\nThe quick brown fox.\n"),
    ("guide.dj", "# Pumps\n\nThe quick brown fox.\n"),
    ("letter.rtf", "{\\rtf1\\ansi The quick brown fox.\\par}"),
];

#[test]
fn markup_a_grammar_also_claims_is_indexed_as_a_document() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = offline_config();
    for (name, body) in MARKUP {
        fs::write(dir.path().join(name), body).unwrap();
    }

    let (report, _) = scan_collecting(dir.path(), &cfg);

    let store = Store::open(dir.path(), basemind::store::VIEW_WORKING).unwrap();
    for (name, _) in MARKUP {
        assert!(
            store.lookup_doc(name).is_some(),
            "{name} is a document (stats: {:?})",
            report.stats
        );
        assert!(store.lookup(name).is_none(), "{name} must leave the code map");
    }
}

#[test]
fn markup_stays_in_the_code_map_without_the_document_tier_or_under_an_override() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("page.html"), MARKUP[0].1).unwrap();
    fs::write(dir.path().join("show.erb"), MARKUP[0].1).unwrap();
    let mut cfg = offline_config();
    cfg.languages.insert(
        "html".to_string(),
        toml::from_str(r#"extensions = [".erb"]"#).expect("language override"),
    );

    scan_collecting(dir.path(), &cfg);
    let store = Store::open(dir.path(), basemind::store::VIEW_WORKING).unwrap();
    assert!(
        store.lookup("show.erb").is_some(),
        "an override's extension keeps its grammar"
    );
    assert!(store.lookup_doc("page.html").is_some());
    drop(store);

    cfg.documents.enabled = false;
    scan_collecting(dir.path(), &cfg);
    let store = Store::open(dir.path(), basemind::store::VIEW_WORKING).unwrap();
    assert_eq!(store.lookup("page.html").map(|e| e.language.as_str()), Some("html"));
}

fn git(root: &Path, args: &[&str]) {
    let status = basemind::testing::git_command()
        .args(args)
        .current_dir(root)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

/// A staged or rev view cannot read a document's own content through xberg (it extracts from a
/// file), so such a view indexes no document at all rather than the working tree's version.
#[test]
fn staged_and_rev_views_index_no_documents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let cfg = offline_config();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    fs::write(root.join("guide.md"), "# Guide\n\ncommitted text\n").unwrap();
    fs::write(root.join("page.html"), MARKUP[0].1).unwrap();
    fs::write(root.join("a.rs"), "pub fn committed() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);
    fs::write(root.join("guide.md"), "# Guide\n\nworking-tree text\n").unwrap();
    let repo = basemind::git::Repo::discover(root).expect("repo");
    let head = repo.resolve_rev("HEAD").expect("head");

    let views = [
        (basemind::store::VIEW_STAGED.to_string(), ScanSource::Staged(&repo)),
        (
            basemind::store::view_name_for_rev(&head[..7]),
            ScanSource::Rev {
                repo: &repo,
                sha: head.clone(),
            },
        ),
    ];
    for (view, source) in views {
        let mut store = Store::open(root, &view).expect("open view");
        let report = basemind::scanner::scan(
            root,
            &mut store,
            &cfg,
            source,
            EmbedMode::Inline,
            basemind::retrieval_wiring::backend(),
        )
        .expect("scan");
        assert_eq!(report.stats.docs_indexed, 0, "{view}: {:?}", report.stats);
        assert!(store.lookup("a.rs").is_some(), "{view} indexes code");
        assert!(
            store.lookup_doc("guide.md").is_none(),
            "{view} must not index documents"
        );
        assert!(
            store.lookup_doc("page.html").is_none(),
            "{view} must not index documents"
        );
        assert!(store.lookup("page.html").is_none(), "{view}: markup is not code either");
    }
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
    fn a_pdf_without_a_text_layer_is_indexed_empty_and_not_extracted_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = offline_config();
        fs::write(dir.path().join("scan.pdf"), textless_pdf()).unwrap();

        let (report, observer) = scan_collecting(dir.path(), &cfg);

        assert_eq!(report.stats.docs_indexed, 1, "stats: {:?}", report.stats);
        assert_eq!(report.stats.extract_failed, 0, "stats: {:?}", report.stats);
        let chunk_count = observer
            .results()
            .iter()
            .find_map(|result| match &result.status {
                FileStatus::DocIndexed { chunk_count, .. } if result.path == "scan.pdf" => Some(*chunk_count),
                _ => None,
            })
            .expect("scan.pdf is reported as an indexed document");
        assert_eq!(chunk_count, 0, "a PDF with no text layer has nothing to chunk");
        let store = Store::open(dir.path(), basemind::store::VIEW_WORKING).unwrap();
        assert!(store.lookup_doc("scan.pdf").is_some(), "the empty document is tracked");
        drop(store);

        let (report, observer) = scan_collecting(dir.path(), &cfg);

        assert_eq!(report.stats.docs_indexed, 0, "stats: {:?}", report.stats);
        assert_eq!(report.stats.extract_failed, 0, "stats: {:?}", report.stats);
        assert!(report.stats.skipped_unchanged >= 1, "stats: {:?}", report.stats);
        assert!(
            observer
                .results()
                .iter()
                .any(|result| result.path == "scan.pdf" && matches!(result.status, FileStatus::DocUnchanged)),
            "the second scan must settle scan.pdf from its entry, not extract it again"
        );
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
        let arguments = json!({ "query": "fox", "entity": "person" });
        let params = CallToolRequestParams::new("docs_search").with_arguments(arguments.as_object().unwrap().clone());

        let result = service
            .call_tool(params)
            .await
            .expect("a tool result, not a protocol error");

        assert_eq!(
            result.is_error,
            Some(true),
            "an entity filter cannot be answered without the NER tier"
        );
        let message = format!("{:?}", result.content);
        assert!(message.contains("`entity` filter of `docs_search`"), "{message}");
        assert!(message.contains("documents-ner"), "{message}");
        let _ = service.cancel().await;
    }
}
