//! Document-tier sub-configs. Split from `v1.rs` to keep both files under the
//! 1000-line cap once iters 3–7 fill in every kreuzberg capability.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Top-level `[documents]` table. Each sub-config has `#[serde(default)]` so
/// adding a new tier never breaks older TOML files.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentsConfig {
    /// Master switch. Only meaningful when the `documents` cargo feature is compiled in.
    #[serde(default = "DocumentsConfig::default_enabled")]
    pub enabled: bool,
    /// MIME-type allowlist. Empty = accept anything kreuzberg can handle.
    #[serde(default)]
    #[schemars(inner(length(min = 1)))]
    pub mime_allowlist: Vec<String>,
    /// Maximum chunk size in characters.
    #[serde(default = "DocumentsConfig::default_max_characters")]
    #[schemars(range(min = 64))]
    pub max_characters: usize,
    /// Overlap between chunks in characters.
    #[serde(default = "DocumentsConfig::default_overlap")]
    #[schemars(range(min = 0))]
    pub overlap: usize,
    /// Kreuzberg embedding preset name. Defaults to "balanced".
    #[serde(default = "DocumentsConfig::default_embedding_preset")]
    pub embedding_preset: String,
    /// Generate embeddings (`true`) or skip vector storage entirely (`false`).
    #[serde(default = "DocumentsConfig::default_embed")]
    pub embed: bool,
    /// Language detection + preferred languages for chunking / extraction.
    #[serde(default)]
    pub language: DocLanguageConfig,
    /// Cross-encoder reranker applied post-vector-search.
    #[serde(default)]
    pub reranker: RerankerConfig,
    /// Keyword extraction at ingest time.
    #[serde(default)]
    pub keywords: KeywordsConfig,
    /// Named-entity recognition at ingest time.
    #[serde(default)]
    pub ner: NerConfig,
    /// Document summarization at ingest time.
    #[serde(default)]
    pub summarization: SummarizationConfig,
    /// OCR backend selection for image / scanned-PDF inputs.
    #[serde(default)]
    pub ocr: OcrConfig,
    /// Response wire format for the document MCP tools.
    #[serde(default)]
    pub output: OutputConfig,
}

impl DocumentsConfig {
    fn default_enabled() -> bool {
        true
    }
    fn default_max_characters() -> usize {
        1000
    }
    fn default_overlap() -> usize {
        200
    }
    fn default_embedding_preset() -> String {
        "balanced".to_string()
    }
    fn default_embed() -> bool {
        true
    }
}

impl Default for DocumentsConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            mime_allowlist: Vec::new(),
            max_characters: Self::default_max_characters(),
            overlap: Self::default_overlap(),
            embedding_preset: Self::default_embedding_preset(),
            embed: Self::default_embed(),
            language: DocLanguageConfig::default(),
            reranker: RerankerConfig::default(),
            keywords: KeywordsConfig::default(),
            ner: NerConfig::default(),
            summarization: SummarizationConfig::default(),
            ocr: OcrConfig::default(),
            output: OutputConfig::default(),
        }
    }
}

/// Language detection + preferred-language list for the document tier. Named
/// `DocLanguageConfig` to avoid colliding with the per-tree-sitter-language
/// `LanguageConfig` (which is the scanner's per-grammar override map).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocLanguageConfig {
    /// Run kreuzberg's language detector. On by default; flip off when every doc
    /// in the corpus is the same known language.
    #[serde(default = "DocLanguageConfig::default_auto_detect")]
    pub auto_detect: bool,
    /// User-specified preferred languages (ISO 639-3, e.g. `"fra"`, `"deu"`).
    /// Drives chunking-tokenizer choice and biases the detector.
    #[serde(default)]
    pub detected_languages: Vec<String>,
}

impl DocLanguageConfig {
    fn default_auto_detect() -> bool {
        true
    }
}

impl Default for DocLanguageConfig {
    fn default() -> Self {
        Self {
            auto_detect: Self::default_auto_detect(),
            detected_languages: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RerankerConfig {
    /// Master switch — off by default; the reranker model download + per-query
    /// latency means users should opt in explicitly.
    #[serde(default)]
    pub enabled: bool,
    /// Kreuzberg reranker preset name (`bge-reranker-base` is the small default;
    /// `bge-reranker-large` and `bge-reranker-v2-m3` are heavier alternatives).
    #[serde(default = "RerankerConfig::default_preset")]
    pub preset: String,
    /// How many hits to rerank. The vector search returns `top_k` candidates
    /// which the cross-encoder then reorders.
    #[serde(default = "RerankerConfig::default_top_k")]
    pub top_k: usize,
}

impl RerankerConfig {
    fn default_preset() -> String {
        "bge-reranker-base".to_string()
    }
    fn default_top_k() -> usize {
        10
    }
}

impl Default for RerankerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            preset: Self::default_preset(),
            top_k: Self::default_top_k(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeywordsConfig {
    /// Master switch — off by default; YAKE / RAKE add ingest-time CPU cost.
    #[serde(default)]
    pub enabled: bool,
    /// Algorithm: YAKE (statistical, multi-language) or RAKE (rapid automatic
    /// keyword extraction).
    #[serde(default)]
    pub algorithm: KeywordAlgorithm,
    /// Maximum keywords to extract per document.
    #[serde(default = "KeywordsConfig::default_count")]
    pub count: usize,
    /// Optional YAKE tuning (passed through to kreuzberg unchanged).
    #[serde(default)]
    pub yake_params: Option<serde_json::Value>,
    /// Optional RAKE tuning (passed through to kreuzberg unchanged).
    #[serde(default)]
    pub rake_params: Option<serde_json::Value>,
}

impl KeywordsConfig {
    fn default_count() -> usize {
        10
    }
}

impl Default for KeywordsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            algorithm: KeywordAlgorithm::default(),
            count: Self::default_count(),
            yake_params: None,
            rake_params: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum KeywordAlgorithm {
    #[default]
    Yake,
    Rake,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NerConfig {
    /// Master switch — off by default; ONNX backend downloads gline-rs weights on
    /// first use, LLM backend costs API tokens.
    #[serde(default)]
    pub enabled: bool,
    /// Backend selection. `onnx` uses gline-rs locally; `llm` routes through the
    /// shared `[llm]` config.
    #[serde(default)]
    pub backend: NerBackend,
    /// Override the ONNX model name (kreuzberg has a default catalogue when unset).
    #[serde(default)]
    pub model: Option<String>,
    /// Whitelist of entity types to surface (`PERSON`, `LOC`, `ORG`, …). Empty
    /// means "use the backend default".
    #[serde(default)]
    pub entity_types: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum NerBackend {
    #[default]
    Onnx,
    Llm,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SummarizationConfig {
    /// Master switch — off by default.
    #[serde(default)]
    pub enabled: bool,
    /// Strategy: extractive (TextRank, no LLM) or abstractive (routed via `[llm]`).
    #[serde(default)]
    pub strategy: SummarizationStrategy,
    /// Soft cap on summary length in characters.
    #[serde(default = "SummarizationConfig::default_max_chars")]
    pub max_chars: usize,
}

impl SummarizationConfig {
    fn default_max_chars() -> usize {
        500
    }
}

impl Default for SummarizationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            strategy: SummarizationStrategy::default(),
            max_chars: Self::default_max_chars(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum SummarizationStrategy {
    #[default]
    Extractive,
    Abstractive,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OcrConfig {
    /// Backend selection. `tesseract` is the default; `paddle` for CJK-heavy
    /// corpora; `vlm` routes through `[llm]` for vision-language OCR.
    #[serde(default)]
    pub backend: OcrBackend,
    /// Tesseract / PaddleOCR language packs (ISO 639-3 codes like `"eng"`).
    #[serde(default = "OcrConfig::default_languages")]
    pub languages: Vec<String>,
}

impl OcrConfig {
    fn default_languages() -> Vec<String> {
        vec!["eng".to_string()]
    }
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            backend: OcrBackend::default(),
            languages: Self::default_languages(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum OcrBackend {
    #[default]
    Tesseract,
    Paddle,
    Vlm,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    /// Wire format used by document-tier MCP responses.
    #[serde(default)]
    pub format: OutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    #[default]
    Json,
    Toon,
}

/// Shared LLM credentials + model selection. Consumed by every LLM-backed
/// capability (ner-llm, summarization-llm, reranker-llm, VLM OCR).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct LlmConfig {
    /// Provider name (`openai`, `anthropic`, `vllm`, …). Maps to a kreuzberg
    /// liter-llm client at the boundary.
    #[serde(default)]
    pub provider: Option<String>,
    /// Provider-specific model name (`gpt-4o-mini`, `claude-sonnet-4-5`, …).
    #[serde(default)]
    pub model: Option<String>,
    /// API key — either a literal (discouraged; keeps secrets in version control)
    /// or an `{ env = "OPENAI_API_KEY" }` reference. `Unset` short-circuits any
    /// LLM-backed feature.
    #[serde(default)]
    pub api_key: ApiKey,
    /// Override the provider base URL (for self-hosted vLLM, Azure, …).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Sampling temperature. Provider-default when unset.
    #[serde(default)]
    pub temperature: Option<f32>,
}

/// Tri-state API key wrapper. Serde-deserialised as `null` (`Unset`), a plain
/// string (`Literal`), or a `{ env = "NAME" }` table (`Env`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(untagged)]
pub enum ApiKey {
    /// Literal value baked into config. Strongly discouraged for production —
    /// `Env` keeps secrets out of source control.
    Literal(String),
    /// `{ env = "OPENAI_API_KEY" }` — resolved at load time via `std::env::var`.
    Env { env: String },
    /// Missing / `null` — every LLM-backed feature treats this as "disabled".
    #[default]
    Unset,
}

impl ApiKey {
    /// Resolve the credential. `Literal` returns its inner value as-is; `Env`
    /// performs an `env::var` lookup and returns `None` if the variable is
    /// missing or empty; `Unset` always returns `None`.
    pub fn resolve(&self) -> Option<SecretString> {
        match self {
            Self::Literal(s) if !s.is_empty() => Some(SecretString(s.clone())),
            Self::Literal(_) => None,
            Self::Env { env } => match std::env::var(env) {
                Ok(v) if !v.is_empty() => Some(SecretString(v)),
                _ => None,
            },
            Self::Unset => None,
        }
    }
}

/// Newtype wrapping a resolved secret. `Debug` and `Display` always render
/// `"<redacted>"` so spans / panics never leak the value. Use `expose()` when
/// you genuinely need the raw bytes at an API boundary.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a raw string as a secret. The caller is responsible for ensuring
    /// the bytes are not already on disk in plaintext.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Borrow the underlying secret. Use only at the boundary that consumes the
    /// credential (e.g. building an HTTP authorization header).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"<redacted>\"")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_string_redacts_in_debug() {
        let s = SecretString::new("hunter2".to_string());
        assert_eq!(format!("{s:?}"), "\"<redacted>\"");
        assert_eq!(format!("{s}"), "<redacted>");
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn api_key_unset_resolves_to_none() {
        assert!(ApiKey::Unset.resolve().is_none());
    }

    #[test]
    fn api_key_literal_resolves_to_value() {
        let k = ApiKey::Literal("sk-test".to_string());
        let resolved = k.resolve().expect("literal resolves");
        assert_eq!(resolved.expose(), "sk-test");
    }

    #[test]
    fn api_key_literal_empty_resolves_to_none() {
        assert!(ApiKey::Literal(String::new()).resolve().is_none());
    }

    #[test]
    fn api_key_env_reads_environment() {
        // SAFETY: scoped to this single test; the env var name is uniquely
        // namespaced so parallel tests can't observe each other.
        unsafe {
            std::env::set_var("BASEMIND_TEST_API_KEY_PRESENT", "value-123");
        }
        let k = ApiKey::Env {
            env: "BASEMIND_TEST_API_KEY_PRESENT".to_string(),
        };
        let resolved = k.resolve().expect("env resolves");
        assert_eq!(resolved.expose(), "value-123");
        unsafe {
            std::env::remove_var("BASEMIND_TEST_API_KEY_PRESENT");
        }
    }

    #[test]
    fn api_key_env_missing_resolves_to_none() {
        // SAFETY: the variable is removed before the test so the env lookup
        // genuinely fails. Single-test scope avoids races.
        unsafe {
            std::env::remove_var("BASEMIND_TEST_API_KEY_MISSING");
        }
        let k = ApiKey::Env {
            env: "BASEMIND_TEST_API_KEY_MISSING".to_string(),
        };
        assert!(k.resolve().is_none());
    }

    #[test]
    fn api_key_deserialises_literal_string() {
        let k: ApiKey = serde_json::from_str("\"sk-test\"").expect("parse");
        match k {
            ApiKey::Literal(s) => assert_eq!(s, "sk-test"),
            other => panic!("expected Literal, got {other:?}"),
        }
    }

    #[test]
    fn api_key_deserialises_env_table() {
        let k: ApiKey = serde_json::from_str(r#"{"env":"OPENAI_API_KEY"}"#).expect("parse");
        match k {
            ApiKey::Env { env } => assert_eq!(env, "OPENAI_API_KEY"),
            other => panic!("expected Env, got {other:?}"),
        }
    }
}
