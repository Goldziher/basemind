//! Invariants the JSON schema cannot carry for `[documents]`, checked on the merged config so env /
//! CLI / MCP overrides (applied after schema validation) cannot slip past them.

use super::documents::DocumentsConfig;

/// Smallest chunk the chunker accepts; mirrors the schema minimum of `documents.max_characters`.
const MIN_MAX_CHARACTERS: usize = 64;

impl DocumentsConfig {
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
