//! When xberg's structured extractor rejects a file that is really just text, read it as plain text.
//!
//! A monorepo scan (88k files) lost 489 documents to extractor verdicts that say nothing about the file
//! being unreadable: Helm `templates/*.yaml` are Go templates, not YAML (`Parsing error: Failed to
//! parse YAML`, 401 files); a CSV past `security_limits.max_table_cells` (`Table cell limit
//! exceeded`, 40); an SVG or YAML fixture with one entity over 1 MiB (`Entity too long`). All of them
//! are text a human can grep, so [`should_retry_as_text`] picks them out and `extract_doc` re-reads
//! them as `text/plain` through the linear chunker, counting the result as degraded
//! (`docs_text_fallback`). Binary containers, corrupt PDFs and IO errors stay failures.

use xberg::XbergError;

/// True for mime types whose bytes are human-readable text, so a plain-text read is a faithful (if
/// unstructured) rendering. Office formats are excluded on purpose even though their mime strings
/// contain `xml`: a corrupt DOCX read as text is garbage.
pub fn is_plain_text_format(mime: &str) -> bool {
    let mime = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if mime.is_empty() || mime.contains("vnd.") {
        return false;
    }
    mime.starts_with("text/")
        || mime.ends_with("+xml")
        || mime.ends_with("+json")
        || matches!(
            mime.as_str(),
            "application/json"
                | "application/xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/x-toml"
                | "application/csv"
        )
}

/// Whether `error` from a structured extraction of a `mime` file should be retried as `text/plain`:
/// a parse failure or a security-limit verdict, on a text-like format. Everything else (IO, OCR,
/// timeouts, unsupported formats) is a genuine failure.
pub fn should_retry_as_text(mime: Option<&str>, error: &XbergError) -> bool {
    let Some(mime) = mime else { return false };
    if mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("text/plain")
    {
        return false;
    }
    is_plain_text_format(mime) && matches!(error, XbergError::Parsing { .. } | XbergError::Security { .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsing() -> XbergError {
        XbergError::parsing("Failed to parse YAML")
    }

    #[test]
    fn text_like_formats_are_eligible_and_binary_ones_are_not() {
        for mime in [
            "application/x-yaml",
            "text/yaml",
            "text/csv",
            "image/svg+xml",
            "application/json",
            "text/html; charset=utf-8",
            "application/toml",
        ] {
            assert!(is_plain_text_format(mime), "{mime}");
        }
        for mime in [
            "application/pdf",
            "image/png",
            "application/zip",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/vnd.ms-excel",
            "application/vnd.oasis.opendocument.text",
            "",
        ] {
            assert!(!is_plain_text_format(mime), "{mime}");
        }
    }

    #[test]
    fn only_parse_and_security_errors_on_text_formats_retry() {
        assert!(should_retry_as_text(Some("application/x-yaml"), &parsing()));
        assert!(should_retry_as_text(
            Some("text/csv"),
            &XbergError::security("Table cell limit exceeded")
        ));
        assert!(!should_retry_as_text(Some("application/pdf"), &parsing()));
        assert!(!should_retry_as_text(Some("text/plain"), &parsing()));
        assert!(!should_retry_as_text(None, &parsing()));
        let io = XbergError::Io(std::io::Error::other("disk"));
        assert!(!should_retry_as_text(Some("text/csv"), &io));
        assert!(!should_retry_as_text(
            Some("text/csv"),
            &XbergError::validation("bad config")
        ));
    }
}
