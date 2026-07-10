//! Layered config merger. Precedence (highest wins):
//! `Mcp` > `Cli` > `Env` > `File` > `Default`.
//!
//! The merger walks the field tree manually — no derive macro — because the
//! override surface (`DocumentsCliOverrides`) only covers `documents.*` today.
//! Adding a new override field means adding one match arm here and one entry
//! in the provenance ledger.

use super::OutputFormat;
use super::documents::{ApiKey, SummarizationStrategy};
use super::overrides::DocumentsCliOverrides;
use super::source::{ConfigSource, ProvenanceMap};
use super::v1::ConfigV1;

/// Bundle of optional layers applied on top of a `defaults` base. Each layer is
/// an `Option<…>` so callers can omit layers they do not care about (e.g. a
/// CLI-only invocation passes `toml_file = None` + `env = None`).
#[derive(Debug, Default, Clone)]
pub struct ConfigLayers {
    /// Parsed `.basemind/basemind.toml`. `None` means no file on disk.
    pub toml_file: Option<ConfigV1>,
    /// Environment-variable layer (typically populated by clap's `#[arg(env = …)]`
    /// when the CLI flag was absent — clap collapses both into one struct, so
    /// today this is reserved for future tooling that wants to separate them).
    pub env: Option<DocumentsCliOverrides>,
    /// CLI flag layer.
    pub cli: Option<DocumentsCliOverrides>,
}

/// Wrap `ConfigV1` with the per-field provenance trail produced during merging.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: ConfigV1,
    pub provenance: ProvenanceMap,
}

/// Walk the layer stack and produce a fully-resolved config plus a per-field
/// provenance ledger. Fields the layers never touch are recorded as `Default`.
pub fn merge_layers(defaults: ConfigV1, layers: ConfigLayers) -> LoadedConfig {
    let mut config = defaults;
    let mut provenance: ProvenanceMap = ProvenanceMap::new();

    for path in DOCUMENT_LEAVES {
        provenance.insert(path, ConfigSource::Default);
    }

    if let Some(file) = layers.toml_file {
        config = file;
        for path in DOCUMENT_LEAVES {
            provenance.insert(path, ConfigSource::File);
        }
    }

    if let Some(env) = layers.env.as_ref() {
        apply_documents_overrides(&mut config, env, ConfigSource::Env, Some(&mut provenance));
    }

    if let Some(cli) = layers.cli.as_ref() {
        apply_documents_overrides(&mut config, cli, ConfigSource::Cli, Some(&mut provenance));
    }

    LoadedConfig { config, provenance }
}

/// Convenience entry point: no layers → defaults-only resolution.
pub fn defaults_only() -> LoadedConfig {
    merge_layers(ConfigV1::with_defaults(), ConfigLayers::default())
}

/// Dotted-path keys for every `documents.*` field the override struct covers.
/// Keeping this list explicit gives us a stable contract for the provenance
/// ledger that tests can assert against.
const DOCUMENT_LEAVES: &[&str] = &[
    "documents.enabled",
    "documents.max_characters",
    "documents.overlap",
    "documents.embedding_preset",
    "documents.embed",
    "documents.language.auto_detect",
    "documents.language.min_confidence",
    "documents.language.detect_multiple",
    "documents.reranker.enabled",
    "documents.reranker.preset",
    "documents.reranker.top_k",
    "documents.keywords.enabled",
    "documents.keywords.max_keywords",
    "documents.keywords.min_score",
    "documents.ner.enabled",
    "documents.summarization.enabled",
    "documents.summarization.strategy",
    "documents.summarization.max_tokens",
    "documents.output.format",
    "llm.model",
    "llm.api_key",
    "llm.base_url",
    "llm.temperature",
    "llm.timeout_secs",
    "llm.max_retries",
    "llm.max_tokens",
];

/// Apply a `DocumentsCliOverrides` layer onto `config`, optionally recording per-field
/// provenance into `provenance`. Pass `None` for `provenance` when the caller does not
/// care about the ledger (e.g. the MCP per-query override path, which throws the ledger
/// away — skipping the [`ProvenanceMap`] allocation entirely on the common path).
pub(crate) fn apply_documents_overrides(
    config: &mut ConfigV1,
    overrides: &DocumentsCliOverrides,
    source: ConfigSource,
    mut provenance: Option<&mut ProvenanceMap>,
) {
    let d = &mut config.documents;
    if let Some(v) = overrides.enabled {
        d.enabled = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.enabled", source);
        }
    }
    if let Some(v) = overrides.max_characters {
        d.max_characters = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.max_characters", source);
        }
    }
    if let Some(v) = overrides.overlap {
        d.overlap = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.overlap", source);
        }
    }
    if let Some(v) = overrides.embedding_preset.clone() {
        d.embedding_preset = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.embedding_preset", source);
        }
    }
    if let Some(v) = overrides.embed {
        d.embed = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.embed", source);
        }
    }
    if let Some(v) = overrides.language_auto_detect {
        d.language.auto_detect = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.language.auto_detect", source);
        }
    }
    if let Some(v) = overrides.language_min_confidence {
        d.language.min_confidence = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.language.min_confidence", source);
        }
    }
    if let Some(v) = overrides.language_detect_multiple {
        d.language.detect_multiple = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.language.detect_multiple", source);
        }
    }
    if let Some(v) = overrides.reranker_enabled {
        d.reranker.enabled = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.reranker.enabled", source);
        }
    }
    if let Some(v) = overrides.reranker_preset.clone() {
        d.reranker.preset = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.reranker.preset", source);
        }
    }
    if let Some(v) = overrides.reranker_top_k {
        d.reranker.top_k = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.reranker.top_k", source);
        }
    }
    if let Some(v) = overrides.keywords_enabled {
        d.keywords.enabled = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.keywords.enabled", source);
        }
    }
    if let Some(v) = overrides.keywords_max_keywords {
        d.keywords.max_keywords = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.keywords.max_keywords", source);
        }
    }
    if let Some(v) = overrides.keywords_min_score {
        d.keywords.min_score = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.keywords.min_score", source);
        }
    }
    if let Some(v) = overrides.ner_enabled {
        d.ner.enabled = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.ner.enabled", source);
        }
    }
    if let Some(v) = overrides.summarization_enabled {
        d.summarization.enabled = v;
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.summarization.enabled", source);
        }
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
        if applied && let Some(p) = provenance.as_mut() {
            p.insert("documents.summarization.strategy", source);
        }
    }
    if let Some(v) = overrides.summarization_max_tokens {
        d.summarization.max_tokens = Some(v);
        if let Some(p) = provenance.as_mut() {
            p.insert("documents.summarization.max_tokens", source);
        }
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
        if applied && let Some(p) = provenance.as_mut() {
            p.insert("documents.output.format", source);
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
        if let Some(p) = provenance.as_mut() {
            p.insert("llm.model", source);
        }
    }
    if let Some(v) = overrides.llm_api_key.clone() {
        llm.api_key = ApiKey::Literal(v);
        if let Some(p) = provenance.as_mut() {
            p.insert("llm.api_key", source);
        }
    }
    if let Some(v) = overrides.llm_base_url.clone() {
        llm.base_url = Some(v);
        if let Some(p) = provenance.as_mut() {
            p.insert("llm.base_url", source);
        }
    }
    if let Some(v) = overrides.llm_temperature {
        llm.temperature = Some(v);
        if let Some(p) = provenance.as_mut() {
            p.insert("llm.temperature", source);
        }
    }
    if let Some(v) = overrides.llm_timeout_secs {
        llm.timeout_secs = Some(v);
        if let Some(p) = provenance.as_mut() {
            p.insert("llm.timeout_secs", source);
        }
    }
    if let Some(v) = overrides.llm_max_retries {
        llm.max_retries = Some(v);
        if let Some(p) = provenance.as_mut() {
            p.insert("llm.max_retries", source);
        }
    }
    if let Some(v) = overrides.llm_max_tokens {
        llm.max_tokens = Some(v);
        if let Some(p) = provenance.as_mut() {
            p.insert("llm.max_tokens", source);
        }
    }
}

#[cfg(test)]
mod tests {
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
