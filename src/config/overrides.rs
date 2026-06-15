//! `DocumentsCliOverrides` — the single struct that backs both `clap` flags
//! and (in iter 3) MCP per-query params. Every field mirrors a
//! [`DocumentsConfig`](super::DocumentsConfig) field but is wrapped in
//! `Option<T>`, so "not provided" stays distinguishable from "explicitly
//! reset to default".
//!
//! clap flags follow `--documents-<section>-<field>`; env vars follow
//! `BASEMIND_DOCUMENTS_<SECTION>_<FIELD>`. The naming scheme is mechanical
//! so a new field added here lights up in all four surfaces (TOML / CLI /
//! MCP / env) without any further wiring.

use clap::Args;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Optional per-call overrides for the document tier. Backs `#[command(flatten)]`
/// on `ScanArgs` / `ServeArgs` and (iter 3) `#[serde(flatten)]` on MCP request
/// types.
#[derive(Args, Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DocumentsCliOverrides {
    /// Override `documents.enabled` (master switch).
    #[arg(long = "documents-enabled", env = "BASEMIND_DOCUMENTS_ENABLED")]
    pub enabled: Option<bool>,

    /// Override `documents.max_characters` (chunk size).
    #[arg(
        long = "documents-max-characters",
        env = "BASEMIND_DOCUMENTS_MAX_CHARACTERS"
    )]
    pub max_characters: Option<usize>,

    /// Override `documents.overlap` (chunk overlap).
    #[arg(long = "documents-overlap", env = "BASEMIND_DOCUMENTS_OVERLAP")]
    pub overlap: Option<usize>,

    /// Override `documents.embedding_preset`.
    #[arg(
        long = "documents-embedding-preset",
        env = "BASEMIND_DOCUMENTS_EMBEDDING_PRESET"
    )]
    pub embedding_preset: Option<String>,

    /// Override `documents.embed` (write embeddings to LanceDB).
    #[arg(long = "documents-embed", env = "BASEMIND_DOCUMENTS_EMBED")]
    pub embed: Option<bool>,

    /// Override `documents.language.auto_detect`.
    #[arg(
        long = "documents-language-auto-detect",
        env = "BASEMIND_DOCUMENTS_LANGUAGE_AUTO_DETECT"
    )]
    pub language_auto_detect: Option<bool>,

    /// Override `documents.language.min_confidence` (0.0–1.0).
    #[arg(
        long = "documents-language-min-confidence",
        env = "BASEMIND_DOCUMENTS_LANGUAGE_MIN_CONFIDENCE"
    )]
    pub language_min_confidence: Option<f64>,

    /// Override `documents.language.detect_multiple`.
    #[arg(
        long = "documents-language-detect-multiple",
        env = "BASEMIND_DOCUMENTS_LANGUAGE_DETECT_MULTIPLE"
    )]
    pub language_detect_multiple: Option<bool>,

    /// Override `documents.reranker.enabled`.
    #[arg(
        long = "documents-reranker-enabled",
        env = "BASEMIND_DOCUMENTS_RERANKER_ENABLED"
    )]
    pub reranker_enabled: Option<bool>,

    /// Override `documents.reranker.preset`.
    #[arg(
        long = "documents-reranker-preset",
        env = "BASEMIND_DOCUMENTS_RERANKER_PRESET"
    )]
    pub reranker_preset: Option<String>,

    /// Override `documents.reranker.top_k`.
    #[arg(
        long = "documents-reranker-top-k",
        env = "BASEMIND_DOCUMENTS_RERANKER_TOP_K"
    )]
    pub reranker_top_k: Option<usize>,

    /// Override `documents.keywords.enabled`.
    #[arg(
        long = "documents-keywords-enabled",
        env = "BASEMIND_DOCUMENTS_KEYWORDS_ENABLED"
    )]
    pub keywords_enabled: Option<bool>,

    /// Override `documents.keywords.max_keywords` (maximum keywords per document).
    #[arg(
        long = "documents-keywords-max-keywords",
        env = "BASEMIND_DOCUMENTS_KEYWORDS_MAX_KEYWORDS"
    )]
    pub keywords_max_keywords: Option<usize>,

    /// Override `documents.keywords.min_score`.
    #[arg(
        long = "documents-keywords-min-score",
        env = "BASEMIND_DOCUMENTS_KEYWORDS_MIN_SCORE"
    )]
    pub keywords_min_score: Option<f32>,

    /// Override `documents.ner.enabled`.
    #[arg(long = "documents-ner-enabled", env = "BASEMIND_DOCUMENTS_NER_ENABLED")]
    pub ner_enabled: Option<bool>,

    /// Override `documents.summarization.enabled`.
    #[arg(
        long = "documents-summarization-enabled",
        env = "BASEMIND_DOCUMENTS_SUMMARIZATION_ENABLED"
    )]
    pub summarization_enabled: Option<bool>,

    /// Override `documents.summarization.max_chars`.
    #[arg(
        long = "documents-summarization-max-chars",
        env = "BASEMIND_DOCUMENTS_SUMMARIZATION_MAX_CHARS"
    )]
    pub summarization_max_chars: Option<usize>,

    /// Override `documents.output.format` (json / toon).
    #[arg(
        long = "documents-output-format",
        env = "BASEMIND_DOCUMENTS_OUTPUT_FORMAT"
    )]
    pub output_format: Option<String>,
}

impl DocumentsCliOverrides {
    /// Empty override set — every field `None`. Equivalent to `Default::default()`
    /// but spelled out for callers that prefer the explicit constructor.
    pub fn empty() -> Self {
        Self::default()
    }

    /// True when at least one override field is populated. Useful for skipping
    /// the merger fast-path when there is nothing to apply.
    pub fn any(&self) -> bool {
        self.enabled.is_some()
            || self.max_characters.is_some()
            || self.overlap.is_some()
            || self.embedding_preset.is_some()
            || self.embed.is_some()
            || self.language_auto_detect.is_some()
            || self.language_min_confidence.is_some()
            || self.language_detect_multiple.is_some()
            || self.reranker_enabled.is_some()
            || self.reranker_preset.is_some()
            || self.reranker_top_k.is_some()
            || self.keywords_enabled.is_some()
            || self.keywords_max_keywords.is_some()
            || self.keywords_min_score.is_some()
            || self.ner_enabled.is_some()
            || self.summarization_enabled.is_some()
            || self.summarization_max_chars.is_some()
            || self.output_format.is_some()
    }
}
