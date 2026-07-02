//! Shared embedding engine for the memory + documents MCP tools.

use anyhow::{Context, Result, anyhow};
use xberg::embeddings::EMBEDDING_PRESETS;
use xberg::{EmbeddingConfig, EmbeddingModelType};

/// Loaded, ready-to-query embedding engine. `Clone` is cheap (config is stack-only).
#[derive(Clone)]
pub struct SharedEmbedder {
    config: EmbeddingConfig,
    dim: u16,
    model_name: String,
}

impl SharedEmbedder {
    /// Build a `SharedEmbedder` from a named xberg preset.
    pub fn load(preset: &str) -> Result<Self> {
        let meta = EMBEDDING_PRESETS.iter().find(|p| p.name == preset).ok_or_else(|| {
            anyhow!(
                "unknown embedding preset '{preset}'; \
                     available: fast, balanced, quality, multilingual"
            )
        })?;
        let dim = u16::try_from(meta.dimensions)
            .with_context(|| format!("preset '{preset}' dimension {} exceeds u16", meta.dimensions))?;
        let config = EmbeddingConfig {
            model: EmbeddingModelType::Preset {
                name: preset.to_string(),
            },
            normalize: true,
            batch_size: 32,
            show_download_progress: false,
            cache_dir: None,
            acceleration: None,
            max_embed_duration_secs: Some(60),
        };
        Ok(Self {
            config,
            dim,
            model_name: preset.to_string(),
        })
    }

    /// Vector dimension produced by this embedder.
    pub fn dim(&self) -> u16 {
        self.dim
    }

    /// The preset name (e.g. `"balanced"`).
    pub fn model(&self) -> &str {
        &self.model_name
    }

    /// Embed a single text string. Returns a `Vec<f32>` of length `self.dim()`.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if text.is_empty() {
            return Err(anyhow!("embed: input text must not be empty"));
        }
        let mut results = xberg::embeddings::embed_texts(&[text], &self.config)
            .with_context(|| format!("embed_texts(preset={})", self.model_name))?;
        results
            .pop()
            .ok_or_else(|| anyhow!("embed_texts returned empty result"))
    }
}
