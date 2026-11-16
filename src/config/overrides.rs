//! `DocumentsCliOverrides` — the struct that backs the `clap` flags and env vars
//! (it is deliberately NOT part of any MCP schema). Every field mirrors a
//! [`DocumentsConfig`](super::DocumentsConfig) field but is wrapped in
//! `Option<T>`, so "not provided" stays distinguishable from "explicitly
//! reset to default".
//!
//! clap flags follow `--documents-<section>-<field>`; env vars follow
//! `BASEMIND_DOCUMENTS_<SECTION>_<FIELD>`. The naming scheme is mechanical
//! so a new field added here lights up in all four surfaces (TOML / CLI /
//! MCP / env) without any further wiring.

use std::path::Path;

use clap::Args;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::documents::{ApiKey, SummarizationStrategy};
use super::layered::OverrideLayer;
use super::source::{ConfigSource, ProvenanceMap};
use super::v1::ConfigV1;
use super::{ConfigError, LoadedConfig, OutputFormat};

/// Optional per-call overrides for the document tier. Backs `#[command(flatten)]`
/// on `ScanArgs` / `ServeArgs`. Never flattened into an MCP request type: every
/// field would be paid for by every session, and `llm_api_key` is a secret.
#[derive(Args, Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DocumentsCliOverrides {
    /// Override `documents.enabled` (master switch).
    #[arg(long = "documents-enabled", env = "BASEMIND_DOCUMENTS_ENABLED")]
    pub enabled: Option<bool>,

    /// Override `documents.max_characters` (chunk size).
    #[arg(long = "documents-max-characters", env = "BASEMIND_DOCUMENTS_MAX_CHARACTERS")]
    pub max_characters: Option<usize>,

    /// Override `documents.overlap` (chunk overlap).
    #[arg(long = "documents-overlap", env = "BASEMIND_DOCUMENTS_OVERLAP")]
    pub overlap: Option<usize>,

    /// Override `documents.embedding_preset`.
    #[arg(long = "documents-embedding-preset", env = "BASEMIND_DOCUMENTS_EMBEDDING_PRESET")]
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
    #[arg(long = "documents-reranker-enabled", env = "BASEMIND_DOCUMENTS_RERANKER_ENABLED")]
    pub reranker_enabled: Option<bool>,

    /// Override `documents.reranker.preset`.
    #[arg(long = "documents-reranker-preset", env = "BASEMIND_DOCUMENTS_RERANKER_PRESET")]
    pub reranker_preset: Option<String>,

    /// Override `documents.reranker.top_k`.
    #[arg(long = "documents-reranker-top-k", env = "BASEMIND_DOCUMENTS_RERANKER_TOP_K")]
    pub reranker_top_k: Option<usize>,

    /// Override `documents.keywords.enabled`.
    #[arg(long = "documents-keywords-enabled", env = "BASEMIND_DOCUMENTS_KEYWORDS_ENABLED")]
    pub keywords_enabled: Option<bool>,

    /// Override `documents.keywords.max_keywords` (maximum keywords per document).
    #[arg(
        long = "documents-keywords-max-keywords",
        env = "BASEMIND_DOCUMENTS_KEYWORDS_MAX_KEYWORDS"
    )]
    pub keywords_max_keywords: Option<usize>,

    /// Override `documents.keywords.min_score`.
    #[arg(long = "documents-keywords-min-score", env = "BASEMIND_DOCUMENTS_KEYWORDS_MIN_SCORE")]
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

    /// Override `documents.summarization.strategy` (`extractive` / `abstractive`).
    #[arg(
        long = "documents-summarization-strategy",
        env = "BASEMIND_DOCUMENTS_SUMMARIZATION_STRATEGY"
    )]
    pub summarization_strategy: Option<String>,

    /// Override `documents.summarization.max_tokens`.
    #[arg(
        long = "documents-summarization-max-tokens",
        env = "BASEMIND_DOCUMENTS_SUMMARIZATION_MAX_TOKENS"
    )]
    pub summarization_max_tokens: Option<u32>,

    /// Override `documents.output.format` (json / toon).
    #[arg(long = "documents-output-format", env = "BASEMIND_DOCUMENTS_OUTPUT_FORMAT")]
    pub output_format: Option<String>,

    /// Override `llm.model` (liter-llm routing format, e.g. `openai/gpt-4o`).
    #[arg(long = "llm-model", env = "BASEMIND_LLM_MODEL")]
    pub llm_model: Option<String>,

    /// Override `llm.api_key` (literal). Use a shell expansion against an env var
    /// (`--llm-api-key "$OPENAI_API_KEY"`) rather than a hard-coded literal.
    /// `hide_env_values = true` keeps the resolved value out of `--help` output.
    #[arg(long = "llm-api-key", env = "BASEMIND_LLM_API_KEY", hide_env_values = true)]
    pub llm_api_key: Option<String>,

    /// Override `llm.base_url` (for self-hosted vLLM, Azure OpenAI, …).
    #[arg(long = "llm-base-url", env = "BASEMIND_LLM_BASE_URL")]
    pub llm_base_url: Option<String>,

    /// Override `llm.temperature` (sampling temperature, provider-default when unset).
    #[arg(long = "llm-temperature", env = "BASEMIND_LLM_TEMPERATURE")]
    pub llm_temperature: Option<f64>,

    /// Override `llm.timeout_secs` (per-request timeout in seconds).
    #[arg(long = "llm-timeout-secs", env = "BASEMIND_LLM_TIMEOUT_SECS")]
    pub llm_timeout_secs: Option<u64>,

    /// Override `llm.max_retries` (retry budget on transient failures).
    #[arg(long = "llm-max-retries", env = "BASEMIND_LLM_MAX_RETRIES")]
    pub llm_max_retries: Option<u32>,

    /// Override `llm.max_tokens` (maximum tokens to generate).
    #[arg(long = "llm-max-tokens", env = "BASEMIND_LLM_MAX_TOKENS")]
    pub llm_max_tokens: Option<u64>,
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
            || self.summarization_strategy.is_some()
            || self.summarization_max_tokens.is_some()
            || self.output_format.is_some()
            || self.llm_model.is_some()
            || self.llm_api_key.is_some()
            || self.llm_base_url.is_some()
            || self.llm_temperature.is_some()
            || self.llm_timeout_secs.is_some()
            || self.llm_max_retries.is_some()
            || self.llm_max_tokens.is_some()
    }
}

impl OverrideLayer for DocumentsCliOverrides {
    fn apply(&self, config: &mut ConfigV1, source: ConfigSource, provenance: Option<&mut ProvenanceMap>) {
        apply_documents_overrides(config, self, source, provenance);
    }
}

/// [`load_with_layers`](super::load_with_layers) over the clap-parsed override struct: the TOML
/// config (if present) plus optional env / CLI layers, fully resolved with provenance.
///
/// `env_overrides` and `cli_overrides` are accepted separately so future
/// tooling can report "this came from `BASEMIND_*`" vs "this came from
/// `--flag`" distinctly. Today clap collapses both into a single
/// `DocumentsCliOverrides` per command — callers typically pass the parsed
/// `args.documents` as `cli_overrides` and `None` as `env_overrides`.
pub fn load_with_overrides(
    root: &Path,
    env_overrides: Option<DocumentsCliOverrides>,
    cli_overrides: Option<DocumentsCliOverrides>,
) -> Result<LoadedConfig, ConfigError> {
    super::load_with_layers(root, env_overrides, cli_overrides)
}

/// Apply a `DocumentsCliOverrides` layer onto `config`, optionally recording per-field
/// provenance into `provenance`. Pass `None` for `provenance` when the caller does not
/// care about the ledger (e.g. the MCP per-query override path, which throws the ledger
/// away — skipping the [`ProvenanceMap`] allocation entirely on the common path).
fn apply_documents_overrides(
    config: &mut ConfigV1,
    overrides: &DocumentsCliOverrides,
    source: ConfigSource,
    mut provenance: Option<&mut ProvenanceMap>,
) {
    let d = &mut config.documents;
    if let Some(v) = overrides.enabled {
        d.enabled = v;
        record_provenance(&mut provenance, source, "documents.enabled");
    }
    if let Some(v) = overrides.max_characters {
        d.max_characters = v;
        record_provenance(&mut provenance, source, "documents.max_characters");
    }
    if let Some(v) = overrides.overlap {
        d.overlap = v;
        record_provenance(&mut provenance, source, "documents.overlap");
    }
    if let Some(v) = overrides.embedding_preset.clone() {
        d.embedding_preset = v;
        record_provenance(&mut provenance, source, "documents.embedding_preset");
    }
    if let Some(v) = overrides.embed {
        d.embed = v;
        record_provenance(&mut provenance, source, "documents.embed");
    }
    if let Some(v) = overrides.language_auto_detect {
        d.language.auto_detect = v;
        record_provenance(&mut provenance, source, "documents.language.auto_detect");
    }
    if let Some(v) = overrides.language_min_confidence {
        d.language.min_confidence = v;
        record_provenance(&mut provenance, source, "documents.language.min_confidence");
    }
    if let Some(v) = overrides.language_detect_multiple {
        d.language.detect_multiple = v;
        record_provenance(&mut provenance, source, "documents.language.detect_multiple");
    }
    if let Some(v) = overrides.reranker_enabled {
        d.reranker.enabled = v;
        record_provenance(&mut provenance, source, "documents.reranker.enabled");
    }
    if let Some(v) = overrides.reranker_preset.clone() {
        d.reranker.preset = v;
        record_provenance(&mut provenance, source, "documents.reranker.preset");
    }
    if let Some(v) = overrides.reranker_top_k {
        d.reranker.top_k = v;
        record_provenance(&mut provenance, source, "documents.reranker.top_k");
    }
    if let Some(v) = overrides.keywords_enabled {
        d.keywords.enabled = v;
        record_provenance(&mut provenance, source, "documents.keywords.enabled");
    }
    if let Some(v) = overrides.keywords_max_keywords {
        d.keywords.max_keywords = v;
        record_provenance(&mut provenance, source, "documents.keywords.max_keywords");
    }
    if let Some(v) = overrides.keywords_min_score {
        d.keywords.min_score = v;
        record_provenance(&mut provenance, source, "documents.keywords.min_score");
    }
    if let Some(v) = overrides.ner_enabled {
        d.ner.enabled = v;
        record_provenance(&mut provenance, source, "documents.ner.enabled");
    }
    if let Some(v) = overrides.summarization_enabled {
        d.summarization.enabled = v;
        record_provenance(&mut provenance, source, "documents.summarization.enabled");
    }
    if let Some(v) = overrides.summarization_strategy.as_deref() {
        let applied = match v.to_ascii_lowercase().as_str() {
            "extractive" => {
                d.summarization.strategy = SummarizationStrategy::Extractive;
                true
            }
            "abstractive" => {
                d.summarization.strategy = SummarizationStrategy::Abstractive;
                true
            }
            _ => {
                tracing::warn!(value = %v, "unknown summarization_strategy value; ignoring");
                false
            }
        };
        if applied {
            record_provenance(&mut provenance, source, "documents.summarization.strategy");
        }
    }
    if let Some(v) = overrides.summarization_max_tokens {
        d.summarization.max_tokens = Some(v);
        record_provenance(&mut provenance, source, "documents.summarization.max_tokens");
    }
    if let Some(v) = overrides.output_format.as_deref() {
        let applied = match v.to_ascii_lowercase().as_str() {
            "json" => {
                d.output.format = OutputFormat::Json;
                true
            }
            "toon" => {
                d.output.format = OutputFormat::Toon;
                true
            }
            _ => {
                tracing::warn!(value = %v, "unknown output_format value; ignoring");
                false
            }
        };
        if applied {
            record_provenance(&mut provenance, source, "documents.output.format");
        }
    }
    apply_llm_overrides(config, overrides, source, provenance);
}

/// Apply the `llm.*` slice of `DocumentsCliOverrides` onto `config.llm`. Split
/// out so `apply_documents_overrides` stays readable and the LLM branches are
/// easy to audit in isolation (api_key carries the secret-handling rule).
fn apply_llm_overrides(
    config: &mut ConfigV1,
    overrides: &DocumentsCliOverrides,
    source: ConfigSource,
    mut provenance: Option<&mut ProvenanceMap>,
) {
    let llm = &mut config.llm;
    if let Some(v) = overrides.llm_model.clone() {
        llm.model = v;
        record_provenance(&mut provenance, source, "llm.model");
    }
    if let Some(v) = overrides.llm_api_key.clone() {
        llm.api_key = ApiKey::Literal(v);
        record_provenance(&mut provenance, source, "llm.api_key");
    }
    if let Some(v) = overrides.llm_base_url.clone() {
        llm.base_url = Some(v);
        record_provenance(&mut provenance, source, "llm.base_url");
    }
    if let Some(v) = overrides.llm_temperature {
        llm.temperature = Some(v);
        record_provenance(&mut provenance, source, "llm.temperature");
    }
    if let Some(v) = overrides.llm_timeout_secs {
        llm.timeout_secs = Some(v);
        record_provenance(&mut provenance, source, "llm.timeout_secs");
    }
    if let Some(v) = overrides.llm_max_retries {
        llm.max_retries = Some(v);
        record_provenance(&mut provenance, source, "llm.max_retries");
    }
    if let Some(v) = overrides.llm_max_tokens {
        llm.max_tokens = Some(v);
        record_provenance(&mut provenance, source, "llm.max_tokens");
    }
}

/// Record `source` for a single config leaf into the provenance ledger, skipping
/// the bookkeeping entirely when the caller passed `None` (the MCP override path).
fn record_provenance(provenance: &mut Option<&mut ProvenanceMap>, source: ConfigSource, key: &'static str) {
    if let Some(p) = provenance.as_mut() {
        p.insert(key, source);
    }
}

#[cfg(test)]
mod tests {
    use super::super::layered::DOCUMENT_LEAVES;
    use super::*;

    /// Regression for I8: an unknown `output_format` must skip only that field —
    /// it must NOT short-circuit `apply_documents_overrides` before the tail call
    /// to `apply_llm_overrides`, which would silently drop every `llm.*` override.
    #[test]
    fn invalid_output_format_still_applies_llm_overrides() {
        let mut config = ConfigV1::with_defaults();
        let mut overrides = DocumentsCliOverrides::empty();
        overrides.output_format = Some("garbage".to_string());
        overrides.llm_model = Some("gpt-test".to_string());

        let mut provenance = ProvenanceMap::new();
        for path in DOCUMENT_LEAVES {
            provenance.insert(path, ConfigSource::Default);
        }
        apply_documents_overrides(&mut config, &overrides, ConfigSource::Cli, Some(&mut provenance));

        assert_eq!(config.llm.model, "gpt-test");
        assert_eq!(provenance.get("llm.model").copied(), Some(ConfigSource::Cli));
        assert_eq!(
            provenance.get("documents.output.format").copied(),
            Some(ConfigSource::Default)
        );
    }

    /// A valid `output_format` applies the field and records its provenance, and
    /// still threads through to the llm overrides.
    #[test]
    fn valid_output_format_applies_field_and_llm_overrides() {
        let mut config = ConfigV1::with_defaults();
        let mut overrides = DocumentsCliOverrides::empty();
        overrides.output_format = Some("TOON".to_string());
        overrides.llm_model = Some("gpt-test".to_string());

        let mut provenance = ProvenanceMap::new();
        for path in DOCUMENT_LEAVES {
            provenance.insert(path, ConfigSource::Default);
        }
        apply_documents_overrides(&mut config, &overrides, ConfigSource::Cli, Some(&mut provenance));

        assert_eq!(config.documents.output.format, OutputFormat::Toon);
        assert_eq!(
            provenance.get("documents.output.format").copied(),
            Some(ConfigSource::Cli)
        );
        assert_eq!(config.llm.model, "gpt-test");
    }
}
