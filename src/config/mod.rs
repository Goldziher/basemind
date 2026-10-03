mod code;
mod comms;
pub mod daemon;
mod documents;
mod documents_validate;
pub(crate) mod layered;
mod overrides;
mod resources;
pub mod root_guard;
pub mod rules;
mod shells;
mod source;
pub mod trust;
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
pub use resources::{DocumentModelProfile, FootprintKeyword, MaxFootprint, ResourcesConfig};
pub use shells::{ShellsConfig, TerminalChoice, VisualMode};
pub use source::{ConfigSource, ProvenanceMap};
pub use v1::{CodeIntelConfig, ConfigV1, CrawlConfig, LanguageConfig};

pub type Config = ConfigV1;

pub const CONFIG_FILE_NAME: &str = "basemind.toml";
pub const BASEMIND_DIR: &str = ".basemind";
/// Project-level `.config/` convention directory (https://github.com/pi0/config-dir).
pub const CONFIG_CONVENTION_DIR: &str = ".config";
/// Tool-named subdirectory of the convention: `<root>/.config/basemind/`.
pub const CONFIG_CONVENTION_SUBDIR: &str = "basemind";
/// Generic config filename used by the nested convention form: `<root>/.config/basemind/config.toml`.
pub const CONFIG_NESTED_FILE_NAME: &str = "config.toml";

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
    let mut config = parse_str(&raw).map_err(|e| annotate_path(e, &path))?;
    trust::sanitize_repo_config(&mut config, trust::Grants::from_env());
    trust::warn_inert_fields(&config);
    Ok(config)
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
    let loaded = merge_layers(
        ConfigV1::with_defaults(),
        ConfigLayers {
            toml_file,
            env: env_overrides,
            cli: cli_overrides,
        },
    );
    validate_merged(&loaded.config)?;
    Ok(loaded)
}

/// Re-check the invariants the schema and `parse_str` enforce on the file, against the merged
/// result: env / CLI / MCP overrides are applied after the file is validated and would otherwise
/// bypass them.
pub fn validate_merged(config: &Config) -> Result<(), ConfigError> {
    config
        .documents
        .validate()
        .and_then(|()| config.code_search.validate())
        .map_err(ConfigError::SchemaValidation)
}

/// Like [`load`], but a broken file is logged rather than returned: for callers that only want an
/// optional setting out of the config and must still never fail on it. `None` for a missing or
/// unreadable file.
pub fn load_logged(root: &Path) -> Option<Config> {
    match load(root) {
        Ok(config) => Some(config),
        Err(ConfigError::NotFound(_)) => None,
        Err(error) => {
            tracing::warn!(root = %root.display(), %error, "ignoring unreadable basemind.toml");
            None
        }
    }
}

/// [`load_logged`] falling back to per-root defaults.
pub fn load_or_default_logged(root: &Path) -> Config {
    load_logged(root).unwrap_or_else(|| default_for_root(root))
}

/// Resolve the repository root by walking UP from `start` to the nearest ancestor that carries a
/// committed config marker — a root `basemind.toml`, or one under the project-level `.config/`
/// convention (`.config/basemind.toml` / `.config/basemind/config.toml`) — falling back to git
/// discovery, then to `start` unchanged. Lets basemind commands run from a monorepo subfolder
/// attach to the configured root.
///
/// The cache moved out of the repo (it is a machine-global XDG store now), so there is no longer a
/// `.basemind/` directory in the tree to anchor on — the committed `basemind.toml` is the durable
/// in-repo marker of "this is a basemind-managed root".
///
/// The upward `basemind.toml` search is **bounded by the closest enclosing git repository**: it
/// never ascends above that repo's workdir. Without the bound, running from inside a nested subrepo
/// (a git repo checked out inside a polyrepo that has its own root `basemind.toml`) would climb
/// across the subrepo boundary and wrongly attach to the parent polyrepo. The subrepo's own root is
/// the ceiling.
///
/// Precedence:
/// 1. The nearest ancestor of `start` (including `start` itself, up to and including the enclosing
///    git root) that holds a config marker (root `basemind.toml` or a `.config/` convention file).
/// 2. Else the git workdir discovered from `start`.
/// 3. Else `start` unchanged.
///
/// Assumes `start` is already canonicalized by the caller.
pub fn discover_root_with_basemind(start: &Path) -> PathBuf {
    let git_root = crate::git::Repo::discover(start).ok().map(|repo| {
        repo.workdir()
            .canonicalize()
            .unwrap_or_else(|_| repo.workdir().to_path_buf())
    });

    let mut current = start;
    loop {
        if has_config_marker(current) {
            return current.to_path_buf();
        }
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

/// Root for `basemind init`: the project you are initializing, never a parent that merely already
/// holds a `.basemind/`.
///
/// This is deliberately NOT [`discover_root_with_basemind`]. That function *attaches* to an existing
/// index and so walks up to an ancestor `.basemind/`; using it for `init` makes `init` "travel" to a
/// parent polyrepo's root and scaffold there instead of in the current repo. `init` *creates* config,
/// so it anchors to the closest enclosing git repository (the committed repo root the scaffold is
/// meant for), falling back to `start` (typically the cwd) when not inside a git repo.
///
/// Assumes `start` is already canonicalized by the caller.
pub fn init_root(start: &Path) -> PathBuf {
    match crate::git::Repo::discover(start) {
        Ok(repo) => repo.workdir().to_path_buf(),
        Err(_) => start.to_path_buf(),
    }
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

/// Flat project-level `.config/` convention: `<root>/.config/basemind.toml`.
pub fn convention_flat_config_path(root: &Path) -> PathBuf {
    root.join(CONFIG_CONVENTION_DIR).join(CONFIG_FILE_NAME)
}

/// Nested project-level `.config/` convention: `<root>/.config/basemind/config.toml`.
pub fn convention_nested_config_path(root: &Path) -> PathBuf {
    root.join(CONFIG_CONVENTION_DIR)
        .join(CONFIG_CONVENTION_SUBDIR)
        .join(CONFIG_NESTED_FILE_NAME)
}

/// Every location a config may live at, in precedence order: the canonical root file wins, then the
/// legacy in-cache file, then the project-level `.config/` convention (flat `.config/basemind.toml`
/// before the nested `.config/basemind/config.toml`). [`load`] reads the first that exists;
/// [`has_config_marker`] treats any of them as the durable "this is a basemind-managed root" marker.
pub fn config_candidate_paths(root: &Path) -> Vec<PathBuf> {
    vec![
        config_path(root),
        legacy_config_path(root),
        convention_flat_config_path(root),
        convention_nested_config_path(root),
    ]
}

/// True when `root` carries a config file at any [`config_candidate_paths`] location.
///
/// This is the marker root discovery and the workspace-root guard key off, so a project that stores
/// its config under the `.config/` convention is recognized exactly like one with a root
/// `basemind.toml`.
pub fn has_config_marker(root: &Path) -> bool {
    config_candidate_paths(root).iter().any(|path| path.is_file())
}

/// Resolve which config file to read: the first existing [`config_candidate_paths`] entry, else the
/// canonical root path (so a not-found error names the location we tell users to create).
pub fn resolve_config_path(root: &Path) -> PathBuf {
    config_candidate_paths(root)
        .into_iter()
        .find(|path| path.exists())
        .unwrap_or_else(|| config_path(root))
}

pub fn parse_str(raw: &str) -> Result<Config, ConfigError> {
    let config = parse_v1(raw)?;
    config.documents.validate().map_err(ConfigError::SchemaValidation)?;
    Ok(config)
}

fn parse_v1(raw: &str) -> Result<Config, ConfigError> {
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
            let config = serde_json::from_value::<ConfigV1>(json_value).map_err(ConfigError::Deserialize)?;
            rules::validate(&config).map_err(ConfigError::SchemaValidation)?;
            Ok(config)
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
    fn code_intel_precise_resolution_defaults_true_and_parses_override() {
        let default_cfg = parse_str("\"$schema\" = \"v1\"\n").unwrap();
        assert!(
            default_cfg.code_intel.precise_resolution,
            "precise_resolution defaults to true when [code_intel] is absent"
        );
        let overridden = parse_str("\"$schema\" = \"v1\"\n[code_intel]\nprecise_resolution = false\n")
            .expect("[code_intel] precise_resolution is a valid, schema-accepted field");
        assert!(
            !overridden.code_intel.precise_resolution,
            "an explicit precise_resolution = false is honored"
        );
    }

    #[test]
    fn resources_section_parses_embed_batch_size_and_defaults_when_absent() {
        let default_cfg = parse_str("\"$schema\" = \"v1\"\n").unwrap();
        assert_eq!(
            default_cfg.resources.embed_batch_size, 32,
            "embed_batch_size defaults to 32 when [resources] is absent"
        );
        assert_eq!(default_cfg.resources.scan_threads, 0);
        assert_eq!(
            default_cfg.resources.document_models,
            crate::config::DocumentModelProfile::Full
        );
        let overridden = parse_str("\"$schema\" = \"v1\"\n[resources]\nembed_batch_size = 8\n")
            .expect("[resources] embed_batch_size is a valid, schema-accepted field");
        assert_eq!(
            overridden.resources.embed_batch_size, 8,
            "an explicit embed_batch_size = 8 is honored"
        );
    }

    #[test]
    fn discover_root_walks_up_to_ancestor_config_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        std::fs::write(root.join(CONFIG_FILE_NAME), "\"$schema\" = \"v1\"\n").expect("write basemind.toml");
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

    #[test]
    fn resolve_reads_flat_config_convention_when_root_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let path = convention_flat_config_path(root);
        std::fs::create_dir_all(path.parent().expect("convention dir")).expect("mkdir .config");
        std::fs::write(&path, "\"$schema\" = \"v1\"\n[scan]\nmax_file_bytes = 1234\n")
            .expect("write convention config");

        assert_eq!(resolve_config_path(root), path);
        assert_eq!(load(root).expect("convention config loads").scan.max_file_bytes, 1234);
    }

    #[test]
    fn resolve_reads_nested_config_convention_when_root_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let path = convention_nested_config_path(root);
        std::fs::create_dir_all(path.parent().expect("convention dir")).expect("mkdir .config/basemind");
        std::fs::write(&path, "\"$schema\" = \"v1\"\n[scan]\nmax_file_bytes = 4321\n")
            .expect("write nested convention");

        assert_eq!(resolve_config_path(root), path);
        assert_eq!(load(root).expect("nested config loads").scan.max_file_bytes, 4321);
    }

    #[test]
    fn root_config_wins_over_the_convention() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let flat = convention_flat_config_path(root);
        std::fs::create_dir_all(flat.parent().expect("convention dir")).expect("mkdir .config");
        std::fs::write(
            config_path(root),
            "\"$schema\" = \"v1\"\n[scan]\nmax_file_bytes = 4096\n",
        )
        .expect("root");
        std::fs::write(&flat, "\"$schema\" = \"v1\"\n[scan]\nmax_file_bytes = 9999\n").expect("convention");

        assert_eq!(resolve_config_path(root), config_path(root));
        assert_eq!(load(root).expect("load").scan.max_file_bytes, 4096);
    }

    #[test]
    fn flat_convention_wins_over_nested_convention() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let flat = convention_flat_config_path(root);
        let nested = convention_nested_config_path(root);
        std::fs::create_dir_all(flat.parent().expect("convention dir")).expect("mkdir .config");
        std::fs::create_dir_all(nested.parent().expect("nested dir")).expect("mkdir .config/basemind");
        std::fs::write(&flat, "\"$schema\" = \"v1\"\n[scan]\nmax_file_bytes = 1111\n").expect("flat");
        std::fs::write(&nested, "\"$schema\" = \"v1\"\n[scan]\nmax_file_bytes = 2222\n").expect("nested");

        assert_eq!(resolve_config_path(root), flat);
    }

    #[test]
    fn discover_root_walks_up_to_the_convention_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        let path = convention_nested_config_path(&root);
        std::fs::create_dir_all(path.parent().expect("convention dir")).expect("mkdir");
        std::fs::write(&path, "\"$schema\" = \"v1\"\n").expect("write marker");
        let sub = root.join("crates").join("inner");
        std::fs::create_dir_all(&sub).expect("mkdir sub");

        assert_eq!(
            discover_root_with_basemind(&sub),
            root,
            "the .config marker anchors the walk"
        );
    }

    #[test]
    fn has_config_marker_recognizes_every_candidate_location() {
        let bare = tempfile::tempdir().expect("tempdir");
        assert!(!has_config_marker(bare.path()), "a bare directory is not a marker");

        for candidate in ["root", "legacy", "flat", "nested"] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path();
            let path = match candidate {
                "root" => config_path(root),
                "legacy" => legacy_config_path(root),
                "flat" => convention_flat_config_path(root),
                _ => convention_nested_config_path(root),
            };
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, "\"$schema\" = \"v1\"\n").expect("write");
            assert!(has_config_marker(root), "{candidate} config is a marker");
        }
    }
}
