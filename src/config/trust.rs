//! Trust boundary for the repository's own `basemind.toml`.
//!
//! The file is authored by whoever wrote the scanned repository, not by the operator running
//! basemind, so a clone can carry settings that aim the process at something the operator never
//! chose. A setting that reaches outside the repository is gated here, once, on the file layer only:
//! the env / CLI override layers are operator-supplied and are never sanitised. `[llm]` is not gated:
//! nothing reads it any more (see [`deprecated_settings_set`]).

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock, PoisonError};

use super::v1::ConfigV1;

/// Operator opt-in to honour the repository's `[scan] follow_symlinks = true`. A tracked link can
/// point anywhere (`~/.ssh`), so following links is an operator decision, not a repo one.
pub const ALLOW_FOLLOW_SYMLINKS_ENV: &str = "BASEMIND_ALLOW_FOLLOW_SYMLINKS";

/// Operator-side grants, from the engine configuration's trust policy.
#[derive(Debug, Clone, Copy, Default)]
pub struct Grants {
    pub follow_symlinks: bool,
}

impl Grants {
    /// The grants of the installed engine configuration.
    pub fn installed() -> Self {
        Self {
            follow_symlinks: super::engine::with(|config| config.trust.allow_follow_symlinks),
        }
    }
}

/// Strip from the repo-sourced config everything the operator has not granted.
pub fn sanitize_repo_config(config: &mut ConfigV1, grants: Grants) {
    if config.scan.follow_symlinks && !grants.follow_symlinks {
        config.scan.follow_symlinks = false;
        warn_once(format!(
            "ignoring [scan] follow_symlinks = true from basemind.toml: a tracked link could pull files outside \
             the repository into the index; set {ALLOW_FOLLOW_SYMLINKS_ENV}=1 in the environment to honour it"
        ));
    }
}

/// Dotted paths of fields that parse but that nothing reads. They stay in the schema so existing
/// files keep loading.
const INERT_FIELDS: &[&str] = &[
    "watch.live_l2",
    "cache.file_map_lru",
    "mcp.transport",
    "memory.enabled",
    "memory.scope_strategy",
    "memory.default_visibility",
    "comms.enabled",
    "comms.idle_timeout_secs",
    "comms.max_messages_per_room",
    "comms.retention_secs",
    "comms.max_rooms",
    "comms.workspace_root",
    "documents.ocr.backend",
    "documents.ocr.languages",
    "documents.language.preferred_languages",
];

/// Inert fields that `config` sets to a non-default value.
pub fn inert_fields_set(config: &ConfigV1) -> Vec<&'static str> {
    let (Ok(set), Ok(default)) = (
        serde_json::to_value(config),
        serde_json::to_value(ConfigV1::with_defaults()),
    ) else {
        return Vec::new();
    };
    INERT_FIELDS
        .iter()
        .copied()
        .filter(|path| {
            let pointer = format!("/{}", path.replace('.', "/"));
            set.pointer(&pointer) != default.pointer(&pointer)
        })
        .collect()
}

pub fn warn_inert_fields(config: &ConfigV1) {
    for path in inert_fields_set(config) {
        warn_once(format!(
            "basemind.toml sets [{path}], which is reserved and currently has no effect"
        ));
    }
}

/// Removed-feature tables that `config` still carries with content.
pub fn deprecated_sections_set(config: &ConfigV1) -> Vec<&'static str> {
    [
        ("crawl", &config.crawl),
        ("shells", &config.shells),
        ("documents.output", &config.documents.output),
    ]
    .into_iter()
    .filter(|(_, section)| !section.is_empty())
    .map(|(name, _)| name)
    .collect()
}

pub fn warn_deprecated_sections(config: &ConfigV1) {
    for name in deprecated_sections_set(config) {
        warn_once(format!(
            "basemind.toml sets [{name}], which configured a removed feature and is ignored; delete the table"
        ));
    }
    for (_, message) in deprecated_settings(config) {
        warn_once(message.to_string());
    }
}

/// Warn once about each set variable of a removed setting; `is_set` reports whether one is set. The
/// CLI passes the process environment when it builds the engine configuration.
pub fn warn_removed_variables(is_set: impl Fn(&str) -> bool) {
    for name in deprecated_env_set(is_set) {
        warn_once(format!(
            "{name} is set, but the setting it overrode was removed and the variable is ignored; unset it"
        ));
    }
}

/// Environment variables of removed settings. Nothing reads them, so a host that still exports one is
/// warned, as a file that still carries the matching table is.
const DEPRECATED_ENV: &[&str] = &["BASEMIND_DOCUMENTS_OUTPUT_FORMAT"];

/// The [`DEPRECATED_ENV`] variables `is_set` reports as set.
fn deprecated_env_set(is_set: impl Fn(&str) -> bool) -> Vec<&'static str> {
    DEPRECATED_ENV.iter().copied().filter(|name| is_set(name)).collect()
}

/// Settings for document capabilities that were removed (archive extraction, LLM summarization and
/// the `[llm]` credentials only it and LLM NER read), as `(dotted path, warning)`. Unlike a
/// [`DeprecatedSection`](super::DeprecatedSection) these stay typed, so a literal `[llm] api_key`
/// keeps serialising redacted; `[llm]` counts as set by a `--llm-*` flag or `BASEMIND_LLM_*` too.
fn deprecated_settings(config: &ConfigV1) -> Vec<(&'static str, &'static str)> {
    let mut set = Vec::new();
    if config.documents.extract_archives {
        set.push((
            "documents.extract_archives",
            "[documents] extract_archives = true is ignored: archive extraction was removed and archives are \
             always skipped; delete the key",
        ));
    }
    if config.documents.summarization.strategy == super::SummarizationStrategy::Abstractive {
        set.push((
            "documents.summarization.strategy",
            "[documents.summarization] strategy = \"abstractive\" is deprecated: LLM summarization was removed and \
             extractive summaries are produced instead; set strategy = \"extractive\"",
        ));
    }
    let llm_is_set = match (
        serde_json::to_value(&config.llm),
        serde_json::to_value(super::LlmConfig::default()),
    ) {
        (Ok(set), Ok(default)) => set != default,
        _ => false,
    };
    if llm_is_set {
        set.push((
            "llm",
            "[llm] (and the --llm-* flags and BASEMIND_LLM_* variables) is ignored: it only configured LLM \
             summarization and LLM entity recognition, which were removed; delete the table",
        ));
    }
    set
}

/// Dotted paths of the removed-capability settings `config` still carries.
pub fn deprecated_settings_set(config: &ConfigV1) -> Vec<&'static str> {
    deprecated_settings(config).into_iter().map(|(path, _)| path).collect()
}

/// Log a warning the first time this process sees `message`, and say whether it was logged; a CLI
/// invocation loads the config several times and would otherwise repeat it.
pub(crate) fn warn_once(message: String) -> bool {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let first = seen.insert(message.clone());
    if first {
        tracing::warn!("{message}");
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_config(extra: &str) -> ConfigV1 {
        crate::config::parse_str(&format!("\"$schema\" = \"v1\"\n{extra}")).expect("parse")
    }

    #[test]
    fn a_repo_llm_table_is_left_as_written_and_listed_as_deprecated() {
        let mut cfg = repo_config(
            "[llm]\nmodel = \"openai/gpt-4o\"\nbase_url = \"https://llm.example\"\n\
             api_key = { env = \"CORP_LLM_TOKEN\" }\n",
        );
        sanitize_repo_config(&mut cfg, Grants::default());
        assert_eq!(cfg.llm.base_url.as_deref(), Some("https://llm.example"));
        assert_eq!(
            cfg.llm.api_key,
            crate::config::ApiKey::Env {
                env: "CORP_LLM_TOKEN".into()
            }
        );
        assert_eq!(deprecated_settings_set(&cfg), vec!["llm"]);
    }

    #[test]
    fn a_removed_feature_table_is_reported_only_when_it_has_content() {
        assert!(deprecated_sections_set(&repo_config("[crawl]\n")).is_empty());
        assert_eq!(
            deprecated_sections_set(&repo_config("[crawl]\nallow_private_network = true\n")),
            vec!["crawl"]
        );
        assert!(deprecated_sections_set(&repo_config("[shells]\n")).is_empty());
        assert_eq!(
            deprecated_sections_set(&repo_config("[shells]\nvisual = \"window\"\n")),
            vec!["shells"]
        );
        assert!(deprecated_sections_set(&repo_config("[documents.output]\n")).is_empty());
        assert_eq!(
            deprecated_sections_set(&repo_config("[documents.output]\nformat = \"toon\"\n")),
            vec!["documents.output"]
        );
    }

    #[test]
    fn a_removed_setting_variable_is_reported_only_when_set() {
        assert!(deprecated_env_set(|_| false).is_empty());
        assert_eq!(
            deprecated_env_set(|name| name == "BASEMIND_DOCUMENTS_OUTPUT_FORMAT"),
            vec!["BASEMIND_DOCUMENTS_OUTPUT_FORMAT"]
        );
    }

    #[test]
    fn follow_symlinks_needs_the_operator_grant() {
        let mut cfg = repo_config("[scan]\nfollow_symlinks = true\n");
        sanitize_repo_config(&mut cfg, Grants::default());
        assert!(!cfg.scan.follow_symlinks);

        let mut cfg = repo_config("[scan]\nfollow_symlinks = true\n");
        sanitize_repo_config(&mut cfg, Grants { follow_symlinks: true });
        assert!(cfg.scan.follow_symlinks);
    }

    #[test]
    fn inert_fields_are_reported_only_when_non_default() {
        assert!(inert_fields_set(&ConfigV1::with_defaults()).is_empty());
        let cfg = repo_config(
            "[watch]\nlive_l2 = true\n[comms]\nmax_rooms = 3\n[documents.language]\npreferred_languages = [\"fra\"]\n",
        );
        assert_eq!(
            inert_fields_set(&cfg),
            vec![
                "watch.live_l2",
                "comms.max_rooms",
                "documents.language.preferred_languages"
            ]
        );
    }
}
