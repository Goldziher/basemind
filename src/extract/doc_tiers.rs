//! The optional document tiers and what a build without them reports.
//!
//! `documents` extracts text; OCR (`documents-ocr`) and named-entity recognition (`documents-ner`)
//! are separate Cargo features so a source build can leave their native and ONNX dependencies out.
//! xberg skips a post-processor or an OCR pass it was not built with without saying so, so every
//! place a missing tier changes the outcome is named here: a config key that cannot take effect, a
//! search filter that could only ever match nothing, a file that needs OCR to yield any text.

use crate::config::{DocumentsConfig, NerBackend};

/// Cargo feature that compiles Tesseract OCR in.
pub const OCR_FEATURE: &str = "documents-ocr";
/// Cargo feature that compiles the ONNX named-entity backend in.
pub const NER_FEATURE: &str = "documents-ner";

/// Whether this binary can OCR images and scanned PDFs.
pub const fn ocr_compiled() -> bool {
    cfg!(feature = "documents-ocr")
}

/// Whether this binary can run named-entity recognition.
pub const fn ner_compiled() -> bool {
    cfg!(feature = "documents-ner")
}

/// True for a MIME type whose only text comes from OCR: every raster image. SVG is XML and is read
/// as text.
pub fn is_ocr_only_mime(mime_type: &str) -> bool {
    mime_type.starts_with("image/") && mime_type != "image/svg+xml"
}

/// One message per `[documents]` setting that is switched on but cannot take effect in this build.
pub fn unavailable_settings(cfg: &DocumentsConfig) -> Vec<String> {
    unavailable_settings_for(cfg, ner_compiled())
}

fn unavailable_settings_for(cfg: &DocumentsConfig, ner_compiled: bool) -> Vec<String> {
    let mut messages = Vec::new();
    if !cfg.enabled || !cfg.ner.enabled {
        return messages;
    }
    if cfg.ner.backend == NerBackend::Llm {
        messages.push(
            "[documents.ner] backend = \"llm\" is not supported: basemind has no LLM entity backend, so no \
             entities are extracted; set backend = \"onnx\""
                .to_string(),
        );
    } else if !ner_compiled {
        messages.push(format!(
            "[documents.ner] enabled = true has no effect: this basemind binary was built without the \
             `{NER_FEATURE}` Cargo feature, so no entities are extracted; rebuild with `--features {NER_FEATURE}` \
             (the published release binary includes it)"
        ));
    }
    messages
}

/// Log [`unavailable_settings`], each message once per process, and return how many were logged. A
/// daemon rescans on every save, so "once per scan" would repeat the same line all day.
pub fn warn_unavailable_settings(cfg: &DocumentsConfig) -> usize {
    unavailable_settings(cfg)
        .into_iter()
        .filter(|message| crate::config::trust::warn_once(message.clone()))
        .count()
}

/// Why an `entity_category` document-search filter cannot be answered, or `None` when it can.
/// Without the NER tier no document carries entities, so the filter would return an empty result
/// that reads as "no such documents".
pub fn entity_filter_unavailable() -> Option<String> {
    entity_filter_unavailable_for(ner_compiled())
}

fn entity_filter_unavailable_for(ner_compiled: bool) -> Option<String> {
    (!ner_compiled).then(|| {
        format!(
            "the `entity_category` filter requires the `{NER_FEATURE}` feature, which is not compiled into this \
             basemind binary, so no document carries entities. Rebuild with `--features {NER_FEATURE}` (the \
             published release binary includes it), or drop the filter."
        )
    })
}

/// Why a document that extracted to no text is a failure rather than an empty success, or `None`
/// when it is a genuine result. A PDF with no text layer is a scan; only OCR can read it.
pub fn no_text_without_ocr(mime_type: &str, content: &str) -> Option<String> {
    no_text_without_ocr_for(mime_type, content, ocr_compiled())
}

fn no_text_without_ocr_for(mime_type: &str, content: &str, ocr_compiled: bool) -> Option<String> {
    (!ocr_compiled && mime_type == "application/pdf" && content.trim().is_empty()).then(|| {
        format!(
            "no text extracted (OCR not compiled in): the PDF has no text layer and this basemind binary was \
             built without the `{OCR_FEATURE}` Cargo feature; rebuild with `--features {OCR_FEATURE}` (the \
             published release binary includes it)"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NerConfig;

    fn ner_enabled(backend: NerBackend) -> DocumentsConfig {
        DocumentsConfig {
            ner: NerConfig {
                enabled: true,
                backend,
                ..NerConfig::default()
            },
            ..DocumentsConfig::default()
        }
    }

    #[test]
    fn ner_enabled_without_the_tier_names_the_key_and_the_feature() {
        let messages = unavailable_settings_for(&ner_enabled(NerBackend::Onnx), false);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("[documents.ner] enabled"), "{messages:?}");
        assert!(messages[0].contains("documents-ner"), "{messages:?}");
        assert!(unavailable_settings_for(&ner_enabled(NerBackend::Onnx), true).is_empty());
    }

    #[test]
    fn the_llm_ner_backend_is_reported_whether_or_not_the_tier_is_compiled() {
        for compiled in [false, true] {
            let messages = unavailable_settings_for(&ner_enabled(NerBackend::Llm), compiled);
            assert_eq!(messages.len(), 1);
            assert!(
                messages[0].contains("backend = \"llm\" is not supported"),
                "{messages:?}"
            );
            assert!(
                !messages[0].contains("ONNX"),
                "must not promise a fallback: {messages:?}"
            );
        }
    }

    #[test]
    fn nothing_is_reported_while_ner_or_documents_are_off() {
        assert!(unavailable_settings_for(&DocumentsConfig::default(), false).is_empty());
        let disabled = DocumentsConfig {
            enabled: false,
            ..ner_enabled(NerBackend::Llm)
        };
        assert!(unavailable_settings_for(&disabled, false).is_empty());
    }

    #[test]
    fn an_unavailable_setting_is_logged_once_per_process() {
        // No other unit test scans with the LLM backend selected, so the process-wide dedup set has
        // not seen this message yet.
        let cfg = ner_enabled(NerBackend::Llm);
        assert_eq!(warn_unavailable_settings(&cfg), 1);
        assert_eq!(warn_unavailable_settings(&cfg), 0);
    }

    #[test]
    fn entity_filter_is_refused_only_without_the_tier() {
        let message = entity_filter_unavailable_for(false).expect("refused");
        assert!(message.contains("entity_category"), "{message}");
        assert!(message.contains("--features documents-ner"), "{message}");
        assert_eq!(entity_filter_unavailable_for(true), None);
        assert_eq!(entity_filter_unavailable().is_none(), ner_compiled());
    }

    #[test]
    fn a_textless_pdf_is_a_failure_only_without_ocr() {
        let message = no_text_without_ocr_for("application/pdf", " \n", false).expect("failure");
        assert!(
            message.starts_with("no text extracted (OCR not compiled in)"),
            "{message}"
        );
        assert!(message.contains("documents-ocr"), "{message}");
        assert_eq!(no_text_without_ocr_for("application/pdf", " \n", true), None);
        assert_eq!(no_text_without_ocr_for("application/pdf", "text", false), None);
        assert_eq!(no_text_without_ocr_for("text/plain", "", false), None);
    }

    #[test]
    fn raster_images_need_ocr_but_svg_does_not() {
        assert!(is_ocr_only_mime("image/png"));
        assert!(is_ocr_only_mime("image/jpeg"));
        assert!(!is_ocr_only_mime("image/svg+xml"));
        assert!(!is_ocr_only_mime("application/pdf"));
    }
}
