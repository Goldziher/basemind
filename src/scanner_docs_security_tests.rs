//! Security containment tests split from `scanner_docs.rs` for the source-file line cap.

use super::*;

#[test]
fn endnote_mime_is_denied_without_the_enw_extension() {
    assert!(
        is_denied_binary_or_archive(
            Path::new("citations/export.xml"),
            "application/x-endnote+xml",
            &DocumentsConfig::default(),
        ),
        "the MIME guard must independently prevent EndNote XML from reaching xberg"
    );
}

#[test]
fn should_extract_document_rejects_endnote_xml_until_xberg_is_patched() {
    let cfg = DocumentsConfig::default();
    assert!(
        should_extract_document(Path::new("citations/library.enw"), &cfg).is_none(),
        "EndNote XML must not reach xberg while its biblib dependency uses vulnerable quick-xml"
    );
}

/// Archives are never extracted: `extract_archives` is inert, so a checked-in archive is skipped by
/// the document tier whatever the setting, whatever MIME the file name maps to.
#[test]
fn checked_in_archives_are_never_extracted_even_with_extract_archives_true() {
    for extract_archives in [false, true] {
        let cfg = DocumentsConfig {
            extract_archives,
            ..DocumentsConfig::default()
        };
        for name in [
            "data/bundle.zip",
            "data/bundle.tar",
            "data/bundle.tar.gz",
            "data/bundle.gz",
            "data/bundle.tgz",
            "dist/pkg-1.0-py3-none-any.whl",
            "libs/app.jar",
            "data/bundle.7z",
            "data/bundle.rar",
            "DATA/BUNDLE.ZIP",
        ] {
            assert!(
                should_extract_document(Path::new(name), &cfg).is_none(),
                "{name} must not be extracted (extract_archives = {extract_archives})"
            );
        }
        assert!(
            should_extract_document(Path::new("docs/guide.md"), &cfg).is_some(),
            "a plain document is still extracted (extract_archives = {extract_archives})"
        );
    }
}
