//! Layered config merger. Precedence (highest wins):
//! `Mcp` > `Cli` > `Env` > `File` > `Default`.
//!
//! The merger walks the field tree manually — no derive macro — because the
//! override surface (`DocumentsCliOverrides`) only covers `documents.*` today.
//! Adding a new override field means adding one match arm in its
//! [`OverrideLayer`] impl and one entry in the provenance ledger.

use super::source::{ConfigSource, ProvenanceMap};
use super::v1::ConfigV1;

/// One override layer above the file: environment variables or CLI flags. The layer writes each
/// field it sets into the config and records `source` for it in `provenance` when given. Defined
/// here and implemented where the flags are parsed (`DocumentsCliOverrides`), so the merger names
/// no argument parser.
pub trait OverrideLayer {
    /// Apply every field this layer sets onto `config`. `provenance` is `None` when the caller
    /// discards the ledger.
    fn apply(&self, config: &mut ConfigV1, source: ConfigSource, provenance: Option<&mut ProvenanceMap>);
}

/// The layer type of a merge with no override layer. It has no values, so a caller can only pass
/// `None` for it.
#[derive(Debug, Clone, Copy)]
pub enum NoOverrides {}

impl OverrideLayer for NoOverrides {
    fn apply(&self, _: &mut ConfigV1, _: ConfigSource, _: Option<&mut ProvenanceMap>) {
        match *self {}
    }
}

/// Bundle of optional layers applied on top of a `defaults` base. Each layer is
/// an `Option<…>` so callers can omit layers they do not care about (e.g. a
/// CLI-only invocation passes `toml_file = None` + `env = None`).
#[derive(Debug, Clone)]
pub struct ConfigLayers<O> {
    /// Parsed repo-root `basemind.toml`. `None` means no file on disk.
    pub toml_file: Option<ConfigV1>,
    /// Environment-variable layer (typically populated by clap's `#[arg(env = …)]`
    /// when the CLI flag was absent — clap collapses both into one struct, so
    /// today this is reserved for future tooling that wants to separate them).
    pub env: Option<O>,
    /// CLI flag layer.
    pub cli: Option<O>,
}

impl<O> Default for ConfigLayers<O> {
    fn default() -> Self {
        Self {
            toml_file: None,
            env: None,
            cli: None,
        }
    }
}

/// Wrap `ConfigV1` with the per-field provenance trail produced during merging.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: ConfigV1,
    pub provenance: ProvenanceMap,
}

/// Walk the layer stack and produce a fully-resolved config plus a per-field
/// provenance ledger. Fields the layers never touch are recorded as `Default`.
pub fn merge_layers<O: OverrideLayer>(defaults: ConfigV1, layers: ConfigLayers<O>) -> LoadedConfig {
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
        env.apply(&mut config, ConfigSource::Env, Some(&mut provenance));
    }

    if let Some(cli) = layers.cli.as_ref() {
        cli.apply(&mut config, ConfigSource::Cli, Some(&mut provenance));
    }

    LoadedConfig { config, provenance }
}

/// Convenience entry point: no layers → defaults-only resolution.
pub fn defaults_only() -> LoadedConfig {
    merge_layers(ConfigV1::with_defaults(), ConfigLayers::<NoOverrides>::default())
}

/// Dotted-path keys for every `documents.*` field the override struct covers.
/// Keeping this list explicit gives us a stable contract for the provenance
/// ledger that tests can assert against.
pub(super) const DOCUMENT_LEAVES: &[&str] = &[
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
    "llm.model",
    "llm.api_key",
    "llm.base_url",
    "llm.temperature",
    "llm.timeout_secs",
    "llm.max_retries",
    "llm.max_tokens",
];
