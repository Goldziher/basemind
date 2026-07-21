//! Building a liter-llm client from a basemind [`LlmConfig`].
//!
//! Connection parameters (api key, base URL, timeout, retries) go into the liter-llm
//! [`ClientConfig`]; the model string, temperature and max-tokens are per-*request* knobs applied
//! by the turn-loop, not baked into the client. An empty `model` is a configuration error.

use std::sync::Arc;
use std::time::Duration;

use basemind::config::{ApiKey, LlmConfig};
use liter_llm::{ClientConfigBuilder, DefaultClient};

use crate::error::{AgentError, Result};
use crate::model::{LiterModelClient, ModelClient};

/// Build a [`ModelClient`] for a role's [`LlmConfig`].
///
/// The api key is resolved through basemind's `ApiKey` (literal or `{ env = "..." }`); when unset,
/// the builder's `load_env` fallback lets liter-llm read the provider's standard env var. `base_url`
/// pins the provider (local OpenAI-compatible endpoints: Ollama, vLLM, LM Studio).
pub fn build_model_client(config: &LlmConfig) -> Result<Arc<dyn ModelClient>> {
    if config.model.is_empty() {
        return Err(AgentError::Config(
            "role has no model configured (empty `model`)".into(),
        ));
    }

    // An explicit `{ env = "NAME" }` whose variable is unset must fail loudly naming NAME, not ~keep
    // fall through to liter-llm's provider-default env lookup (which could silently pick up an ~keep
    // unrelated key such as OPENAI_API_KEY). ~keep
    let mut builder = match (&config.api_key, config.api_key.resolve()) {
        (_, Some(secret)) => ClientConfigBuilder::new(secret.expose().to_string()),
        (ApiKey::Env { env }, None) => {
            return Err(AgentError::Config(format!(
                "api_key env var `{env}` is unset or empty (model `{}`)",
                config.model
            )));
        }
        (_, None) => ClientConfigBuilder::from_env(),
    };
    if let Some(url) = &config.base_url {
        builder = builder.base_url(url.clone());
    }
    if let Some(secs) = config.timeout_secs {
        builder = builder.timeout(Duration::from_secs(secs));
    }
    if let Some(retries) = config.max_retries {
        builder = builder.max_retries(retries);
    }

    let client = DefaultClient::new(builder.build(), Some(&config.model))?;
    Ok(Arc::new(LiterModelClient::new(Arc::new(client))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_model_is_a_config_error() {
        let cfg = LlmConfig::default();
        // `Arc<dyn ModelClient>` is not `Debug`, so match rather than `expect_err`. ~keep
        assert!(matches!(build_model_client(&cfg), Err(AgentError::Config(_))));
    }

    #[test]
    fn unresolved_env_key_errors_naming_the_var() {
        let cfg = LlmConfig {
            model: "openai/gpt-4o".into(),
            api_key: ApiKey::Env {
                env: "BASEMIND_TEST_DEFINITELY_MISSING_KEY".into(),
            },
            ..Default::default()
        };
        match build_model_client(&cfg) {
            Err(AgentError::Config(message)) => {
                assert!(message.contains("BASEMIND_TEST_DEFINITELY_MISSING_KEY"), "{message}");
            }
            _ => panic!("expected a Config error naming the missing env var"),
        }
    }
}
