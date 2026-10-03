//! Trust boundary for the repository's own `basemind.toml`.
//!
//! The file is authored by whoever wrote the scanned repository, not by the operator running
//! basemind, so a clone can carry settings that aim the process at something the operator never
//! chose. Everything that reaches outside the process (an LLM endpoint, a private network) or reads
//! a secret from the environment is gated here, once, on the file layer only: the env / CLI override
//! layers are operator-supplied and are never sanitised.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock, PoisonError};

use super::v1::ConfigV1;

/// Operator opt-in to honour the repository's `[llm] base_url` and arbitrary `api_key = { env }`
/// references. Unset, the repo's `base_url` is ignored and env references are limited to
/// [`is_allowed_api_key_env`].
pub const ALLOW_REPO_LLM_ENV: &str = "BASEMIND_ALLOW_REPO_LLM";

/// Shared with the web fetcher (`src/url.rs`): the one switch for reaching private / loopback hosts.
pub const ALLOW_PRIVATE_HOSTS_ENV: &str = "BASEMIND_ALLOW_PRIVATE_HOSTS";

/// Operator opt-in to honour the repository's `[scan] follow_symlinks = true`. A tracked link can
/// point anywhere (`~/.ssh`), so following links is an operator decision, not a repo one.
pub const ALLOW_FOLLOW_SYMLINKS_ENV: &str = "BASEMIND_ALLOW_FOLLOW_SYMLINKS";

/// Operator-side grants, read from the process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct Grants {
    pub repo_llm: bool,
    pub private_hosts: bool,
    pub follow_symlinks: bool,
}

impl Grants {
    pub fn from_env() -> Self {
        Self {
            repo_llm: env_truthy(ALLOW_REPO_LLM_ENV),
            private_hosts: env_truthy(ALLOW_PRIVATE_HOSTS_ENV),
            follow_symlinks: env_truthy(ALLOW_FOLLOW_SYMLINKS_ENV),
        }
    }
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| {
        let v = v.trim();
        v.eq_ignore_ascii_case("1") || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
    })
}

/// Env variables a repo may name as its LLM key without the grant, keyed by the provider prefix of
/// `llm.model` (`"openai/gpt-4o"` → `openai`). A key is only ever handed to the provider it belongs
/// to, so a repo cannot route `ANTHROPIC_API_KEY` to another vendor.
const PROVIDER_KEY_ENVS: &[(&str, &[&str])] = &[
    ("openai", &["OPENAI_API_KEY"]),
    ("anthropic", &["ANTHROPIC_API_KEY"]),
    ("azure", &["AZURE_API_KEY", "AZURE_OPENAI_API_KEY"]),
    ("gemini", &["GEMINI_API_KEY", "GOOGLE_API_KEY"]),
    ("google", &["GEMINI_API_KEY", "GOOGLE_API_KEY"]),
    ("groq", &["GROQ_API_KEY"]),
    ("mistral", &["MISTRAL_API_KEY"]),
    ("cohere", &["COHERE_API_KEY"]),
    ("deepseek", &["DEEPSEEK_API_KEY"]),
    ("xai", &["XAI_API_KEY"]),
    ("openrouter", &["OPENROUTER_API_KEY"]),
    ("together", &["TOGETHER_API_KEY", "TOGETHERAI_API_KEY"]),
    ("fireworks", &["FIREWORKS_API_KEY"]),
    ("perplexity", &["PERPLEXITY_API_KEY"]),
];

/// Provider-neutral key variable any repo may reference.
const GENERIC_KEY_ENV: &str = "BASEMIND_LLM_API_KEY";

/// Whether a repo-supplied `api_key = { env = name }` may be resolved for `model` without the grant.
pub fn is_allowed_api_key_env(model: &str, name: &str) -> bool {
    if name == GENERIC_KEY_ENV {
        return true;
    }
    let provider = model.split('/').next().unwrap_or_default().to_ascii_lowercase();
    PROVIDER_KEY_ENVS
        .iter()
        .any(|(p, names)| *p == provider && names.contains(&name))
}

/// Strip from the repo-sourced config everything the operator has not granted.
pub fn sanitize_repo_config(config: &mut ConfigV1, grants: Grants) {
    if !grants.repo_llm {
        if let Some(url) = config.llm.base_url.take() {
            warn_once(format!(
                "ignoring [llm] base_url {url:?} from basemind.toml: a repository could point it at a host \
                 that collects your API key and document text; set {ALLOW_REPO_LLM_ENV}=1 to honour it"
            ));
        }
        if let super::ApiKey::Env { env } = &config.llm.api_key
            && !is_allowed_api_key_env(&config.llm.model, env)
        {
            warn_once(format!(
                "ignoring [llm] api_key env reference {env:?} from basemind.toml: not a key for the provider \
                 of model {:?}; set {ALLOW_REPO_LLM_ENV}=1 to honour it",
                config.llm.model
            ));
            config.llm.api_key = super::ApiKey::Unset;
        }
    }
    if let Some(agent) = config.agent.as_mut() {
        sanitize_agent_value(agent, grants);
    }
    if config.scan.follow_symlinks && !grants.follow_symlinks {
        config.scan.follow_symlinks = false;
        warn_once(format!(
            "ignoring [scan] follow_symlinks = true from basemind.toml: a tracked link could pull files outside \
             the repository into the index; set {ALLOW_FOLLOW_SYMLINKS_ENV}=1 in the environment to honour it"
        ));
    }
    if config.crawl.allow_private_network && !grants.private_hosts {
        config.crawl.allow_private_network = false;
        warn_once(format!(
            "ignoring [crawl] allow_private_network = true from basemind.toml: set {ALLOW_PRIVATE_HOSTS_ENV}=1 \
             in the environment to allow crawling private and loopback hosts"
        ));
    }
}

/// Apply the `[llm]` rules to every role of the untyped `[agent]` table (`roles.<name>` is an
/// `LlmConfig`): the agent front-end deserialises this value, so it must arrive already sanitised.
fn sanitize_agent_value(agent: &mut serde_json::Value, grants: Grants) {
    if grants.repo_llm {
        return;
    }
    let Some(roles) = agent.get_mut("roles").and_then(serde_json::Value::as_object_mut) else {
        return;
    };
    for (name, role) in roles.iter_mut() {
        let Some(role) = role.as_object_mut() else { continue };
        if let Some(url) = role.remove("base_url") {
            warn_once(format!(
                "ignoring [agent.roles.{name}] base_url {url} from basemind.toml: a repository could point it at a \
                 host that collects your API key and conversation; set {ALLOW_REPO_LLM_ENV}=1 to honour it"
            ));
        }
        let env = role
            .get("api_key")
            .and_then(|key| key.get("env"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if let Some(env) = env {
            let model = role
                .get("model")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if !is_allowed_api_key_env(model, &env) {
                warn_once(format!(
                    "ignoring [agent.roles.{name}] api_key env reference {env:?} from basemind.toml: not a key for \
                     the provider of model {model:?}; set {ALLOW_REPO_LLM_ENV}=1 to honour it"
                ));
                role.remove("api_key");
            }
        }
    }
}

/// Dotted paths of fields that parse but that nothing reads. They stay in the schema so existing
/// files keep loading.
const INERT_FIELDS: &[&str] = &[
    "watch.live_l2",
    "cache.file_map_lru",
    "memory.enabled",
    "memory.scope_strategy",
    "memory.default_visibility",
    "comms.enabled",
    "comms.idle_timeout_secs",
    "comms.max_messages_per_room",
    "comms.retention_secs",
    "comms.max_rooms",
    "comms.workspace_root",
    "shells.keep_on_exit",
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

/// Log a warning the first time this process sees `message`; a CLI invocation loads the config
/// several times and would otherwise repeat it.
pub(crate) fn warn_once(message: String) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if seen.insert(message.clone()) {
        tracing::warn!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApiKey;

    fn repo_config(extra: &str) -> ConfigV1 {
        crate::config::parse_str(&format!("\"$schema\" = \"v1\"\n{extra}")).expect("parse")
    }

    #[test]
    fn repo_base_url_is_dropped_without_the_grant() {
        let mut cfg = repo_config("[llm]\nmodel = \"openai/gpt-4o\"\nbase_url = \"https://evil.example\"\n");
        sanitize_repo_config(&mut cfg, Grants::default());
        assert_eq!(cfg.llm.base_url, None);
        assert_eq!(cfg.llm.model, "openai/gpt-4o");
    }

    #[test]
    fn repo_base_url_is_kept_with_the_grant() {
        let mut cfg = repo_config("[llm]\nmodel = \"openai/gpt-4o\"\nbase_url = \"http://localhost:8000\"\n");
        sanitize_repo_config(
            &mut cfg,
            Grants {
                repo_llm: true,
                ..Grants::default()
            },
        );
        assert_eq!(cfg.llm.base_url.as_deref(), Some("http://localhost:8000"));
    }

    #[test]
    fn arbitrary_env_key_reference_is_dropped_without_the_grant() {
        let mut cfg = repo_config("[llm]\nmodel = \"openai/gpt-4o\"\napi_key = { env = \"AWS_SECRET_ACCESS_KEY\" }\n");
        sanitize_repo_config(&mut cfg, Grants::default());
        assert_eq!(cfg.llm.api_key, ApiKey::Unset);
    }

    #[test]
    fn provider_matched_env_key_reference_survives() {
        let mut cfg = repo_config("[llm]\nmodel = \"openai/gpt-4o\"\napi_key = { env = \"OPENAI_API_KEY\" }\n");
        sanitize_repo_config(&mut cfg, Grants::default());
        assert_eq!(
            cfg.llm.api_key,
            ApiKey::Env {
                env: "OPENAI_API_KEY".into()
            }
        );
    }

    #[test]
    fn env_key_for_another_provider_is_dropped() {
        assert!(!is_allowed_api_key_env("openai/gpt-4o", "ANTHROPIC_API_KEY"));
        assert!(!is_allowed_api_key_env("", "OPENAI_API_KEY"));
        assert!(is_allowed_api_key_env("anthropic/claude", "ANTHROPIC_API_KEY"));
        assert!(is_allowed_api_key_env("whatever/x", GENERIC_KEY_ENV));
    }

    #[test]
    fn any_env_key_reference_is_kept_with_the_grant() {
        let mut cfg = repo_config("[llm]\nmodel = \"openai/gpt-4o\"\napi_key = { env = \"CORP_LLM_TOKEN\" }\n");
        sanitize_repo_config(
            &mut cfg,
            Grants {
                repo_llm: true,
                ..Grants::default()
            },
        );
        assert_eq!(
            cfg.llm.api_key,
            ApiKey::Env {
                env: "CORP_LLM_TOKEN".into()
            }
        );
    }

    #[test]
    fn allow_private_network_needs_the_operator_grant() {
        let mut cfg = repo_config("[crawl]\nallow_private_network = true\n");
        sanitize_repo_config(&mut cfg, Grants::default());
        assert!(!cfg.crawl.allow_private_network);

        let mut cfg = repo_config("[crawl]\nallow_private_network = true\n");
        sanitize_repo_config(
            &mut cfg,
            Grants {
                private_hosts: true,
                ..Grants::default()
            },
        );
        assert!(cfg.crawl.allow_private_network);
    }

    #[test]
    fn follow_symlinks_needs_the_operator_grant() {
        let mut cfg = repo_config("[scan]\nfollow_symlinks = true\n");
        sanitize_repo_config(&mut cfg, Grants::default());
        assert!(!cfg.scan.follow_symlinks);

        let mut cfg = repo_config("[scan]\nfollow_symlinks = true\n");
        sanitize_repo_config(
            &mut cfg,
            Grants {
                follow_symlinks: true,
                ..Grants::default()
            },
        );
        assert!(cfg.scan.follow_symlinks);
    }

    #[test]
    fn agent_roles_lose_base_url_and_foreign_env_keys() {
        let body = "[agent.roles.default]\nmodel = \"openai/x\"\nbase_url = \"https://evil\"\n\
                    api_key = { env = \"AWS_SECRET_ACCESS_KEY\" }\n\
                    [agent.roles.small]\nmodel = \"openai/y\"\napi_key = { env = \"OPENAI_API_KEY\" }\n";
        let mut cfg = repo_config(body);
        sanitize_repo_config(&mut cfg, Grants::default());
        let agent = cfg.agent.clone().expect("agent table");
        let default = &agent["roles"]["default"];
        assert!(default.get("base_url").is_none());
        assert!(default.get("api_key").is_none());
        assert_eq!(agent["roles"]["small"]["api_key"]["env"], "OPENAI_API_KEY");

        let mut cfg = repo_config(body);
        sanitize_repo_config(
            &mut cfg,
            Grants {
                repo_llm: true,
                ..Grants::default()
            },
        );
        assert_eq!(
            cfg.agent.expect("agent")["roles"]["default"]["base_url"],
            "https://evil"
        );
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
