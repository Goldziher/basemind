mod code;
mod comms;
mod documents;
pub(crate) mod layered;
mod overrides;
mod shells;
mod source;
mod v1;
mod validate;

use std::path::{Path, PathBuf};

use thiserror::Error;

pub use code::CodeSearchConfig;
pub use comms::CommsConfig;
pub use documents::{
    ApiKey, DocLanguageConfig, DocumentsConfig, KeywordAlgorithm, KeywordsConfig, LlmConfig, NerBackend, NerConfig,
    OcrBackend, OcrConfig, OutputConfig, OutputFormat, RerankerConfig, SecretString, SummarizationConfig,
    SummarizationStrategy,
};
pub use layered::{ConfigLayers, LoadedConfig, defaults_only, merge_layers};
pub use overrides::DocumentsCliOverrides;
pub use shells::{ShellsConfig, TerminalChoice, VisualMode};
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
    let path = resolve_config_path(root);
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

/// Resolve the repository root by walking UP from `start` to the nearest ancestor that already
/// holds a `.basemind/` cache dir (monorepo/nested-git support), falling back to git discovery,
/// then to `start` unchanged. Lets basemind commands run from a monorepo subfolder attach to the
/// root `.basemind` index.
///
/// The upward `.basemind` search is **bounded by the closest enclosing git repository**: it never
/// ascends above that repo's workdir. Without the bound, running from inside a nested subrepo (a
/// git repo checked out inside a polyrepo that has its own root `.basemind`) would climb across the
/// subrepo boundary and wrongly attach to the parent polyrepo's cache. The subrepo's own root is
/// the ceiling.
///
/// Precedence:
/// 1. The nearest ancestor of `start` (including `start` itself, up to and including the enclosing
///    git root) whose `.basemind/` is a directory.
/// 2. Else the git workdir discovered from `start`.
/// 3. Else `start` unchanged.
///
/// Assumes `start` is already canonicalized by the caller.
pub fn discover_root_with_basemind(start: &Path) -> PathBuf {
    // Closest enclosing git repo workdir — the ceiling for the `.basemind` walk. Canonicalized so
    // it compares equal to the canonical `current` path as we ascend. `None` when `start` is not
    // inside any git repo: then there is no repo boundary to respect and the walk runs to the
    // filesystem root (non-git monorepo case).
    let git_root = crate::git::Repo::discover(start).ok().map(|repo| {
        repo.workdir()
            .canonicalize()
            .unwrap_or_else(|_| repo.workdir().to_path_buf())
    });

    let mut current = start;
    loop {
        if current.join(BASEMIND_DIR).is_dir() {
            return current.to_path_buf();
        }
        // Stop after checking the enclosing git root — do not ascend past it into a parent repo.
        if git_root.as_deref() == Some(current) {
            break;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => break,
        }
    }
    git_root.unwrap_or_else(|| start.to_path_buf())
}

/// Canonical (write) location of the config: `<root>/basemind.toml`.
///
/// The config moved from inside `.basemind/` to the repo root so it can be committed — the
/// `.basemind/` cache is wiped on every schema-version bump and is gitignored, which made an
/// in-cache config non-durable. `basemind init` writes here; [`resolve_config_path`] still reads
/// the legacy in-cache location for back-compat.
pub fn config_path(root: &Path) -> PathBuf {
    root.join(CONFIG_FILE_NAME)
}

/// Legacy config location: `<root>/.basemind/basemind.toml`. Read-only fallback kept so existing
/// checkouts that still carry an in-cache config keep loading it until the user re-runs
/// `basemind init` (which writes the root location).
pub fn legacy_config_path(root: &Path) -> PathBuf {
    root.join(BASEMIND_DIR).join(CONFIG_FILE_NAME)
}

/// Resolve which config file to read: prefer the canonical root `basemind.toml`, else the legacy
/// in-cache path, else the root path (so a not-found error names the location we tell users to
/// create). The root file always wins when both exist.
pub fn resolve_config_path(root: &Path) -> PathBuf {
    let root_path = config_path(root);
    if root_path.exists() {
        return root_path;
    }
    let legacy = legacy_config_path(root);
    if legacy.exists() {
        return legacy;
    }
    root_path
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extra_roots_through_schema_validation() {
        let raw = "\"$schema\" = \"v1\"\n[scan]\nextra_roots = [\"/opt/ext\", \"/var/cache/bazel\"]\n";
        let cfg = parse_str(raw).expect("extra_roots is a valid, schema-accepted scan field");
        assert_eq!(
            cfg.scan.extra_roots,
            vec![PathBuf::from("/opt/ext"), PathBuf::from("/var/cache/bazel"),]
        );
    }

    #[test]
    fn extra_roots_defaults_to_empty() {
        let cfg = parse_str("\"$schema\" = \"v1\"\n").unwrap();
        assert!(cfg.scan.extra_roots.is_empty());
    }

    #[test]
    fn discover_root_walks_up_to_ancestor_basemind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        std::fs::create_dir(root.join(BASEMIND_DIR)).expect("mkdir .basemind");
        let sub = root.join("a").join("b");
        std::fs::create_dir_all(&sub).expect("mkdir sub");
        assert_eq!(discover_root_with_basemind(&sub), root);
    }

    #[test]
    fn discover_root_returns_start_when_no_basemind_or_git() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let start = tmp.path().canonicalize().expect("canonicalize");
        assert_eq!(discover_root_with_basemind(&start), start);
    }
}
