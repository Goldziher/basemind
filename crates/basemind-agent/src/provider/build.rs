//! Building a liter-llm client from a basemind [`LlmConfig`].
//!
//! Connection parameters (api key, base URL, timeout, retries) go into the liter-llm
//! [`ClientConfig`]; the model string, temperature and max-tokens are per-*request* knobs applied
//! by the turn-loop, not baked into the client. An empty `model` is a configuration error.

use std::sync::Arc;
use std::time::Duration;

use basemind::config::LlmConfig;
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

    let mut builder = match config.api_key.resolve() {
        Some(secret) => ClientConfigBuilder::new(secret.expose().to_string()),
        None => ClientConfigBuilder::from_env(),
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
        // `Arc<dyn ModelClient>` is not `Debug`, so match rather than `expect_err`.
        assert!(matches!(build_model_client(&cfg), Err(AgentError::Config(_))));
    }
}
