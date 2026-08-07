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
