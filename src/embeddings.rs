//! Shared embedding engine for the memory + documents MCP tools.

use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use xberg::embeddings::EMBEDDING_PRESETS;
use xberg::{EmbeddingConfig, EmbeddingModelType};

/// Global bounded rayon `ThreadPool` for all ONNX embed calls. Initialized once
/// on first use; subsequent calls to `embed_pool` return the same pool regardless
/// of the `max_threads` argument (the pool size is fixed for the process).
static EMBED_POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();

/// Resolve the embedding thread cap.
///
/// `0` is the sentinel for "auto": `max(2, logical_cpus / 4)` — a bounded
/// fraction of available cores that leaves the full global rayon pool free for
/// code-map extraction and prevents the embedder from pinning all cores.
/// Any non-zero value is used directly.
pub fn resolve_embed_threads(max_threads: usize) -> usize {
    if max_threads == 0 {
        std::cmp::max(2, rayon::current_num_threads() / 4)
    } else {
        max_threads
    }
}

/// Returns the process-wide bounded rayon `ThreadPool` for ONNX embedding.
///
/// Initialized once on first call with `resolve_embed_threads(max_threads)`.
/// All `embed_texts` calls from [`SharedEmbedder`] run inside this pool via
/// `pool.install(...)`, which constrains xberg's internal rayon tasks (including
/// the per-chunk embedding fan-out) to at most `current_num_threads()` workers.
pub fn embed_pool(max_threads: usize) -> &'static rayon::ThreadPool {
    EMBED_POOL.get_or_init(|| {
        let n = resolve_embed_threads(max_threads);
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .thread_name(|i| format!("bm-embed-{i}"))
            .build()
            .expect("failed to build embedding rayon pool")
    })
}

/// Loaded, ready-to-query embedding engine. `Clone` is cheap (config is stack-only).
#[derive(Clone)]
pub struct SharedEmbedder {
    config: EmbeddingConfig,
    dim: u16,
    model_name: String,
    /// Resolved embed-thread cap passed through to `embed_pool`. `0` triggers the
    /// auto heuristic inside `resolve_embed_threads`; first caller wins for the
    /// global pool.
    max_embed_threads: usize,
}

impl SharedEmbedder {
    /// Build a `SharedEmbedder` from a named xberg preset.
    ///
    /// `max_embed_threads` bounds the process-wide embedding pool (see
    /// [`embed_pool`]). Pass `0` to use the auto heuristic (`max(2, cores/4)`).
    pub fn load(preset: &str, max_embed_threads: usize) -> Result<Self> {
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
            max_sequence_length: None,
        };
        Ok(Self {
            config,
            dim,
            model_name: preset.to_string(),
            max_embed_threads,
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
    ///
    /// The call is routed through the process-wide bounded [`embed_pool`] so it
    /// cannot saturate the global rayon pool used by the code-map scanner.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if text.is_empty() {
            return Err(anyhow!("embed: input text must not be empty"));
        }
        embed_pool(self.max_embed_threads).install(|| {
            let mut results = xberg::embeddings::embed_texts(&[text], &self.config)
                .with_context(|| format!("embed_texts(preset={})", self.model_name))?;
            results
                .pop()
                .ok_or_else(|| anyhow!("embed_texts returned empty result"))
        })
    }

    /// Embed a batch of texts in one call. Returns one `Vec<f32>` of length `self.dim()` per
    /// input, in order. Used by the code-search scanner to embed a file's chunks in bulk.
    ///
    /// Errors if any input text is empty (xberg rejects empty strings, which produce meaningless
    /// embeddings). An empty batch returns an empty vector without touching the model.
    ///
    /// The call is routed through the process-wide bounded [`embed_pool`] so it
    /// cannot saturate the global rayon pool used by the code-map scanner.
    #[cfg(feature = "code-search")]
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        embed_pool(self.max_embed_threads).install(|| {
            xberg::embeddings::embed_texts(texts, &self.config)
                .with_context(|| format!("embed_texts(preset={}, batch={})", self.model_name, texts.len()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_embed_threads_nonzero_passthrough() {
        assert_eq!(resolve_embed_threads(4), 4);
        assert_eq!(resolve_embed_threads(1), 1);
        assert_eq!(resolve_embed_threads(16), 16);
    }

    #[test]
    fn resolve_embed_threads_zero_gives_auto() {
        let got = resolve_embed_threads(0);
        let expected = std::cmp::max(2, rayon::current_num_threads() / 4);
        assert_eq!(
            got, expected,
            "resolve_embed_threads(0) should yield max(2, cores/4) = {expected}"
        );
        assert!(got >= 2, "auto embed cap must be >= 2, got {got}");
    }
}
