//! Invariants the JSON schema cannot carry for `[documents]`, checked on the merged config so env /
//! CLI / MCP overrides (applied after schema validation) cannot slip past them.

use super::documents::DocumentsConfig;

/// Smallest chunk the chunker accepts; mirrors the schema minimum of `documents.max_characters`.
const MIN_MAX_CHARACTERS: usize = 64;

/// Smallest accepted splitter cutover; mirrors the schema minimum of the `*_chunk_max_bytes` knobs.
const MIN_CHUNK_CUTOVER_BYTES: u64 = 1024;

/// Largest accepted `max_abandoned_extractions`; mirrors the schema maximum.
pub const MAX_ABANDONED_EXTRACTIONS: usize = 64;

impl DocumentsConfig {
    /// The document-phase budget actually enforced: `phase_budget_secs`, but never below
    /// `extraction_timeout_secs`, so a raised per-file timeout is not silently cut short by the default
    /// phase budget (a single document may legitimately use its whole per-file budget).
    pub fn effective_phase_budget_secs(&self) -> u64 {
        self.phase_budget_secs.max(self.extraction_timeout_secs)
    }

    /// Returns a human-readable error naming the offending values on violation.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_characters < MIN_MAX_CHARACTERS {
            return Err(format!(
                "[documents] max_characters ({}) must be at least {MIN_MAX_CHARACTERS}",
                self.max_characters
            ));
        }
        if self.overlap >= self.max_characters {
            return Err(format!(
                "[documents] overlap ({}) must be less than max_characters ({}); an overlap \
                 >= max_characters collapses the chunker step to 1 character",
                self.overlap, self.max_characters
            ));
        }
        for (name, value) in [
            ("markdown_chunk_max_bytes", self.markdown_chunk_max_bytes),
            ("plain_text_chunk_max_bytes", self.plain_text_chunk_max_bytes),
        ] {
            if value < MIN_CHUNK_CUTOVER_BYTES {
                return Err(format!(
                    "[documents] {name} ({value}) must be at least {MIN_CHUNK_CUTOVER_BYTES}"
                ));
            }
        }
        if self.extraction_timeout_secs == 0 {
            return Err("[documents] extraction_timeout_secs must be at least 1".to_string());
        }
        if self.phase_budget_secs == 0 {
            return Err("[documents] phase_budget_secs must be at least 1".to_string());
        }
        if !(1..=MAX_ABANDONED_EXTRACTIONS).contains(&self.max_abandoned_extractions) {
            return Err(format!(
                "[documents] max_abandoned_extractions ({}) must be between 1 and {MAX_ABANDONED_EXTRACTIONS}",
                self.max_abandoned_extractions
            ));
        }
        let confidence = self.language.min_confidence;
        if !(0.0..=1.0).contains(&confidence) {
            return Err(format!(
                "[documents.language] min_confidence ({confidence}) must be between 0.0 and 1.0"
            ));
        }
        if self.reranker.top_k == 0 {
            return Err("[documents.reranker] top_k must be at least 1".to_string());
        }
        if self.keywords.max_keywords == 0 {
            return Err("[documents.keywords] max_keywords must be at least 1".to_string());
        }
        if let [min, max] = self.keywords.ngram_range[..] {
            if min == 0 || min > max {
                return Err(format!(
                    "[documents.keywords] ngram_range [{min}, {max}] must satisfy 1 <= min <= max"
                ));
            }
        } else {
            return Err("[documents.keywords] ngram_range must have exactly two entries".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DocLanguageConfig, KeywordsConfig};

    #[test]
    fn defaults_are_valid() {
        DocumentsConfig::default().validate().expect("defaults validate");
    }

    #[test]
    fn rejects_tiny_max_characters_overlap_and_confidence() {
        let d = DocumentsConfig {
            max_characters: 10,
            overlap: 0,
            ..Default::default()
        };
        assert!(d.validate().unwrap_err().contains("max_characters"));

        let d = DocumentsConfig {
            overlap: DocumentsConfig::default().max_characters,
            ..Default::default()
        };
        assert!(d.validate().unwrap_err().contains("overlap"));

        let d = DocumentsConfig {
            language: DocLanguageConfig {
                min_confidence: 7.0,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(d.validate().unwrap_err().contains("min_confidence"));
    }

    #[test]
    fn rejects_chunk_cutover_below_minimum() {
        for (markdown, plain, name) in [
            (1023, 4096, "markdown_chunk_max_bytes"),
            (4096, 0, "plain_text_chunk_max_bytes"),
        ] {
            let d = DocumentsConfig {
                markdown_chunk_max_bytes: markdown,
                plain_text_chunk_max_bytes: plain,
                ..Default::default()
            };
            assert!(d.validate().unwrap_err().contains(name), "{name}");
        }
    }

    #[test]
    fn rejects_zero_extraction_timeout() {
        let d = DocumentsConfig {
            extraction_timeout_secs: 0,
            ..Default::default()
        };
        assert!(d.validate().unwrap_err().contains("extraction_timeout_secs"));
    }

    #[test]
    fn rejects_zero_phase_budget_and_out_of_range_abandoned_cap() {
        let d = DocumentsConfig {
            phase_budget_secs: 0,
            ..Default::default()
        };
        assert!(d.validate().unwrap_err().contains("phase_budget_secs"));
        for cap in [0, MAX_ABANDONED_EXTRACTIONS + 1] {
            let d = DocumentsConfig {
                max_abandoned_extractions: cap,
                ..Default::default()
            };
            assert!(d.validate().unwrap_err().contains("max_abandoned_extractions"), "{cap}");
        }
        assert!(DocumentsConfig::default().validate().is_ok());
    }

    #[test]
    fn the_phase_budget_is_never_below_the_per_file_timeout() {
        let d = DocumentsConfig {
            extraction_timeout_secs: 3600,
            phase_budget_secs: 1800,
            ..Default::default()
        };
        assert!(d.validate().is_ok(), "an existing large timeout must stay valid");
        assert_eq!(d.effective_phase_budget_secs(), 3600);
        assert_eq!(DocumentsConfig::default().effective_phase_budget_secs(), 1800);
    }

    #[test]
    fn rejects_unordered_ngram_range() {
        let d = DocumentsConfig {
            keywords: KeywordsConfig {
                ngram_range: vec![3, 1],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(d.validate().unwrap_err().contains("ngram_range"));
    }
}
