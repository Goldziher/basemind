mod documents;
mod layered;
mod migrate;
mod overrides;
mod source;
mod v1;
mod validate;

use std::path::{Path, PathBuf};

use thiserror::Error;

pub use documents::{
    ApiKey, DocLanguageConfig, DocumentsConfig, KeywordAlgorithm, KeywordsConfig, LlmConfig,
    NerBackend, NerConfig, OcrBackend, OcrConfig, OutputConfig, OutputFormat, RerankerConfig,
    SecretString, SummarizationConfig, SummarizationStrategy,
};
pub use layered::{ConfigLayers, LoadedConfig, defaults_only, merge_layers};
pub use overrides::DocumentsCliOverrides;
pub use source::{ConfigSource, ProvenanceMap};
pub use v1::{ConfigV1, CrawlConfig};

pub type Config = ConfigV1;

pub const CONFIG_FILE_NAME: &str = "basemind.toml";
pub const BASEMIND_DIR: &str = ".basemind";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file not found at {0}")]
    NotFound(PathBuf),
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid TOML in {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("config is missing required \"$schema\" field — add `\"$schema\" = \"v1\"`")]
    MissingSchema,
    #[error("unknown schema version {0:?} — supported: v1")]
    UnknownSchema(String),
    #[error("schema validation failed:\n{0}")]
    SchemaValidation(String),
    #[error("config does not match v1 shape after validation: {0}")]
    Deserialize(#[source] serde_json::Error),
}

/// Load the TOML config (no overrides). Existing call sites use this — the
/// layered merger is reached via [`load_with_overrides`] when CLI flags or
/// env vars are involved.
pub fn load(root: &Path) -> Result<Config, ConfigError> {
    let path = config_path(root);
    if !path.exists() {
        return Err(ConfigError::NotFound(path));
    }
    let raw = std::fs::read_to_string(&path).map_err(|source| ConfigError::Io {
        path: path.clone(),
        source,
    })?;
    parse_str(&raw).map_err(|e| annotate_path(e, &path))
}

/// Load the TOML config (if present) plus optional env / CLI override layers,
/// produce a fully-resolved `LoadedConfig` with provenance.
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
    let toml_file = match load(root) {
        Ok(cfg) => Some(cfg),
        Err(ConfigError::NotFound(_)) => None,
        Err(e) => return Err(e),
    };
    Ok(merge_layers(
        ConfigV1::with_defaults(),
        ConfigLayers {
            toml_file,
            env: env_overrides,
            cli: cli_overrides,
        },
    ))
}

pub fn config_path(root: &Path) -> PathBuf {
    root.join(BASEMIND_DIR).join(CONFIG_FILE_NAME)
}

pub fn parse_str(raw: &str) -> Result<Config, ConfigError> {
    let toml_value: toml::Value = toml::from_str(raw).map_err(|source| ConfigError::Toml {
        path: PathBuf::new(),
        source,
    })?;
    let json_value: serde_json::Value =
        serde_json::to_value(&toml_value).expect("toml::Value → serde_json::Value never fails");

    let schema_tag = json_value
        .as_object()
        .and_then(|o| o.get("$schema"))
        .and_then(|v| v.as_str())
        .ok_or(ConfigError::MissingSchema)?;

    match schema_tag {
        "v1" | "https://basemind.dev/schema/v1.json" => {
            validate::validate_v1(&json_value)?;
            serde_json::from_value::<ConfigV1>(json_value).map_err(ConfigError::Deserialize)
        }
        other => Err(ConfigError::UnknownSchema(other.to_string())),
    }
}

pub fn default_for_root(_root: &Path) -> Config {
    ConfigV1::with_defaults()
}

fn annotate_path(err: ConfigError, path: &Path) -> ConfigError {
    match err {
        ConfigError::Toml { source, .. } => ConfigError::Toml {
            path: path.to_path_buf(),
            source,
        },
        other => other,
    }
}
