//! Document extraction tier — non-source files (PDFs, Office docs, emails,
//! images, …) ingested via `kreuzberg::extract_file_sync` and serialised to
//! `.basemind/blobs/<hash>.doc.msgpack`.
//!
//! Layered on top of the existing `l1` / `l2` blob shape:
//! - `l1`/`l2`/`l3` cover source code (tree-sitter outlines + calls + body hashes)
//! - `doc` covers everything else (PDFs, DOCX, XLSX, EML, HTML, images via OCR, …)
//!
//! When the document feature is on, each extracted chunk carries its embedding
//! vector inline so the scanner can stage it for LanceDB insert without a second
//! pass through the embedding engine.

use std::path::Path;

use kreuzberg::LanguageDetectionConfig;
use kreuzberg::core::config::ExtractionConfig;
use kreuzberg::core::config::processing::{ChunkingConfig, EmbeddingConfig};
use kreuzberg::core::extractor::extract_file_sync;
use serde::{Deserialize, Serialize};

use super::{ExtractError, SCHEMA_VER};
use crate::config::{
    DocLanguageConfig, KeywordAlgorithm, KeywordsConfig, LlmConfig, NerBackend, NerConfig,
    SummarizationConfig, SummarizationStrategy,
};

/// Per-file document extraction result. Mirrors the shape of `FileMapL1` —
/// `schema_ver` for migration, plus the structured kreuzberg output we care
/// about for downstream vector search.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileMapDoc {
    pub schema_ver: u16,
    /// IANA MIME type as reported by kreuzberg's detector.
    pub mime_type: String,
    /// Plain-text representation of the document (concatenation of all chunks
    /// before chunking is applied; not exactly the source bytes).
    pub content: String,
    /// Document-level metadata (author, title, dates, format-specific keys).
    /// Flattened to `String -> String` so the on-disk shape stays stable.
    pub metadata: Vec<(String, String)>,
    /// ISO 639-3 language codes detected in the content, when language
    /// detection succeeded. (Kreuzberg's wrapper around `whatlang` normalises
    /// every detected variant to its three-letter ISO 639-3 code — see
    /// `kreuzberg::language_detection::lang_to_iso639_3`.)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detected_languages: Vec<String>,
    /// Chunks, each with its embedding vector inline. Empty when chunking is
    /// disabled in the kreuzberg config; embedding fields empty when the
    /// embedding engine is not configured.
    pub chunks: Vec<DocChunk>,
    /// Name of the embedding model that produced the vectors. Empty when no
    /// embeddings were generated. Used by the LanceDB layer to detect
    /// model-change wipes.
    pub embedding_model: String,
    /// Length of each chunk embedding vector. 0 when no embeddings.
    pub embedding_dim: u16,
    /// Keywords extracted from `content` when keyword analysis is enabled.
    ///
    /// Appended at the TAIL of the struct so msgpack positional decoding stays
    /// backward-compatible: older `.doc.msgpack` blobs deserialize via
    /// `#[serde(default)]`, surfacing an empty vec without forcing a schema bump.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<DocKeyword>,
    /// Named entities detected in `content` by the NER backend (or empty when NER is off).
    ///
    /// TAIL field for the same reason as `keywords` — additive within the
    /// minor-version schema policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<DocEntity>,
    /// Document-level summary produced by the summarisation post-processor.
    /// `None` when summarisation was disabled at scan time or when kreuzberg
    /// declined to produce one (e.g. empty content, abstractive strategy with
    /// no LLM model configured).
    ///
    /// TAIL field — pre-iter-7 blobs deserialise via `#[serde(default)]` and
    /// surface as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<DocSummary>,
}

/// Mirror of `kreuzberg::keywords::Keyword`, narrowed to the fields we persist.
/// We do not re-export kreuzberg's `Keyword` directly because we control the
/// on-disk blob shape and want a forward-compatible string for `algorithm`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocKeyword {
    /// Verbatim keyword span.
    pub text: String,
    /// Backend-reported score. YAKE scores lower-is-better; RAKE higher-is-better.
    pub score: f32,
    /// `"yake"` or `"rake"` — the kreuzberg `KeywordAlgorithm` variant stringified
    /// so consumers don't need to depend on the kreuzberg enum.
    pub algorithm: String,
}

/// Mirror of `kreuzberg::types::entity::Entity` with `EntityCategory` flattened
/// to a string. Flattening keeps the blob shape forward-compatible: kreuzberg
/// can add `EntityCategory` variants without invalidating our cached blobs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocEntity {
    /// Lowercase category name — `"person"`, `"organization"`, `"location"`,
    /// `"date"`, `"time"`, `"money"`, `"percent"`, `"email"`, `"phone"`,
    /// `"url"`, or any caller-supplied custom label.
    pub category: String,
    /// Raw mention text exactly as it appeared in `content`.
    pub text: String,
    /// Byte-offset span start in `content`.
    pub start: u32,
    /// Byte-offset span end in `content` (exclusive).
    pub end: u32,
    /// Backend-reported confidence in `[0.0, 1.0]`. `None` when the backend does
    /// not expose confidence scores (e.g. some LLM modes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

/// Mirror of `kreuzberg::DocumentSummary` with `SummaryStrategy` flattened to
/// a string. Flattening keeps the blob shape forward-compatible: kreuzberg can
/// add `SummaryStrategy` variants without invalidating our cached blobs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocSummary {
    /// Plain-prose summary text.
    pub text: String,
    /// Strategy that produced this summary — `"extractive"` (TextRank) or
    /// `"abstractive"` (LLM).
    pub strategy: String,
    /// Approximate token count of `text`, when the backend reports one. `None`
    /// when the backend (typically the extractive path) does not measure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_count: Option<u32>,
}

/// A single chunked region of a document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocChunk {
    /// UTF-8 byte offset where this chunk starts in the original text.
    pub byte_start: u32,
    /// UTF-8 byte offset where this chunk ends.
    pub byte_end: u32,
    /// The chunk text. Stored even when an embedding is present so MCP search
    /// can return snippets without round-tripping to the source file.
    pub text: String,
    /// Embedding vector. Empty when chunking ran without an embedding config.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embedding: Vec<f32>,
}

/// Caller-supplied knobs for document extraction.
///
/// Kept independent from kreuzberg's full `ExtractionConfig` so the scanner
/// callsite stays readable; we translate to `ExtractionConfig` at the boundary.
#[derive(Debug, Clone)]
pub struct DocConfig {
    pub max_characters: usize,
    pub overlap: usize,
    pub embedding_preset: Option<String>,
    pub embed: bool,
    pub language: DocLanguageConfig,
    pub keywords: KeywordsConfig,
    pub ner: NerConfig,
    /// Summarisation knobs (`enabled`, `strategy`, `max_tokens`).
    pub summarization: SummarizationConfig,
    /// Shared LLM credentials reached for when `summarization.strategy = Abstractive`
    /// (and, in future iters, by `ner.backend = Llm`, VLM OCR, etc.).
    pub llm: LlmConfig,
}

impl Default for DocConfig {
    fn default() -> Self {
        Self {
            max_characters: 1000,
            overlap: 200,
            embedding_preset: Some("balanced".to_string()),
            embed: true,
            language: DocLanguageConfig::default(),
            keywords: KeywordsConfig::default(),
            ner: NerConfig::default(),
            summarization: SummarizationConfig::default(),
            llm: LlmConfig::default(),
        }
    }
}

impl DocConfig {
    fn to_kreuzberg(&self) -> ExtractionConfig {
        let embedding = if self.embed {
            Some(EmbeddingConfig::default())
        } else {
            None
        };
        let chunking = ChunkingConfig {
            max_characters: self.max_characters,
            overlap: self.overlap,
            embedding,
            preset: self.embedding_preset.clone(),
            ..Default::default()
        };
        // Kreuzberg rc.10's `ChunkingConfig` has no language input — sentence /
        // word boundaries fall out of `ChunkerType` + `ChunkSizing` rather than
        // a tokenizer keyed on language. Only `LanguageDetectionConfig` is
        // wired here; an iter 5+ change can revisit chunker selection if
        // upstream gains a language hint.
        // kreuzberg gates detection on Option::is_some at features.rs:311; `None`
        // is the "off" signal, not Some { enabled: false }.
        let language_detection = if self.language.auto_detect {
            Some(LanguageDetectionConfig {
                enabled: true,
                min_confidence: self.language.min_confidence,
                detect_multiple: self.language.detect_multiple,
            })
        } else {
            None
        };
        let keywords = self.kreuzberg_keywords();
        let ner = self.kreuzberg_ner();
        let summarization = self.kreuzberg_summarization();
        ExtractionConfig {
            chunking: Some(chunking),
            language_detection,
            keywords,
            ner,
            summarization,
            ..Default::default()
        }
    }

    /// Translate the basemind-side `SummarizationConfig` into kreuzberg's
    /// `SummarizationConfig`. Returns `None` when summarisation is gated off —
    /// kreuzberg treats `ExtractionConfig.summarization == None` as "do not run".
    ///
    /// When `strategy = Abstractive` and `[llm].model` is empty, we fall back to
    /// `Extractive` (TextRank, no LLM) with a one-time warning. This keeps the
    /// scan completing instead of failing midway with an opaque liter-llm error
    /// the agent can't act on.
    fn kreuzberg_summarization(&self) -> Option<kreuzberg::SummarizationConfig> {
        if !self.summarization.enabled {
            return None;
        }
        let mut sc = kreuzberg::SummarizationConfig {
            strategy: match self.summarization.strategy {
                SummarizationStrategy::Extractive => kreuzberg::SummaryStrategy::Extractive,
                SummarizationStrategy::Abstractive => kreuzberg::SummaryStrategy::Abstractive,
            },
            max_tokens: self.summarization.max_tokens,
            llm: None,
        };
        if matches!(
            self.summarization.strategy,
            SummarizationStrategy::Abstractive
        ) {
            sc.llm = self.llm.to_kreuzberg();
            if sc.llm.is_none() {
                tracing::warn!(
                    "summarization.strategy = abstractive but llm.model unset; falling back to extractive"
                );
                sc.strategy = kreuzberg::SummaryStrategy::Extractive;
            }
        }
        Some(sc)
    }

    /// Translate the basemind-side `KeywordsConfig` into kreuzberg's
    /// `KeywordConfig`. Returns `None` when keyword extraction is gated off —
    /// kreuzberg treats `ExtractionConfig.keywords == None` as "do not run".
    ///
    /// `yake_params` / `rake_params` are typed pass-through: bad JSON is logged
    /// and dropped (kreuzberg defaults take over) instead of failing the scan.
    fn kreuzberg_keywords(&self) -> Option<kreuzberg::KeywordConfig> {
        if !self.keywords.enabled {
            return None;
        }
        // Defend against `ngram_range` not being length-2 (schema constraint enforces
        // it; the runtime fallback keeps a malformed in-memory config from panicking).
        let ngram = if self.keywords.ngram_range.len() == 2 {
            (self.keywords.ngram_range[0], self.keywords.ngram_range[1])
        } else {
            (1, 3)
        };
        let mut kc = kreuzberg::KeywordConfig {
            algorithm: match self.keywords.algorithm {
                KeywordAlgorithm::Yake => kreuzberg::KeywordAlgorithm::Yake,
                KeywordAlgorithm::Rake => kreuzberg::KeywordAlgorithm::Rake,
            },
            max_keywords: self.keywords.max_keywords,
            min_score: self.keywords.min_score,
            ngram_range: ngram,
            language: None,
            yake_params: None,
            rake_params: None,
        };
        if let Some(v) = self.keywords.yake_params.as_ref() {
            match serde_json::from_value::<kreuzberg::keywords::YakeParams>(v.clone()) {
                Ok(p) => kc.yake_params = Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "invalid yake_params; using kreuzberg defaults")
                }
            }
        }
        if let Some(v) = self.keywords.rake_params.as_ref() {
            match serde_json::from_value::<kreuzberg::keywords::RakeParams>(v.clone()) {
                Ok(p) => kc.rake_params = Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "invalid rake_params; using kreuzberg defaults")
                }
            }
        }
        // Surface algorithm/params mismatches loudly — kreuzberg silently ignores
        // YakeParams when the algorithm is Rake (and vice versa). The user almost
        // certainly meant for the params to apply; logging once at config-build
        // time lets them spot the typo without parsing the kreuzberg source.
        if self.keywords.yake_params.is_some() && self.keywords.algorithm != KeywordAlgorithm::Yake
        {
            tracing::warn!(
                algorithm = ?self.keywords.algorithm,
                "yake_params set but algorithm is not Yake; params ignored"
            );
        }
        if self.keywords.rake_params.is_some() && self.keywords.algorithm != KeywordAlgorithm::Rake
        {
            tracing::warn!(
                algorithm = ?self.keywords.algorithm,
                "rake_params set but algorithm is not Rake; params ignored"
            );
        }
        Some(kc)
    }

    /// Translate the basemind-side `NerConfig` into kreuzberg's
    /// `core::config::NerConfig`. `None` when NER is gated off.
    ///
    /// String category names round-trip via `EntityCategory::from(String)` —
    /// unknown names land in the `Custom(_)` variant rather than failing.
    ///
    /// When `backend == Llm`, the shared `LlmConfig` is resolved via
    /// `to_kreuzberg()` and threaded into the kreuzberg-side `NerConfig.llm`.
    /// If the user selected the LLM backend but left `llm.model` empty, we
    /// emit a warning — kreuzberg silently falls back to ONNX in that case
    /// and the user almost certainly wants to know.
    fn kreuzberg_ner(&self) -> Option<kreuzberg::core::config::ner::NerConfig> {
        if !self.ner.enabled {
            return None;
        }
        let llm = if matches!(self.ner.backend, NerBackend::Llm) {
            let cfg = self.llm.to_kreuzberg();
            if cfg.is_none() {
                tracing::warn!(
                    "ner.backend = llm but llm.model is unset; NER will fall back to ONNX inside kreuzberg"
                );
            }
            cfg
        } else {
            None
        };
        Some(kreuzberg::core::config::ner::NerConfig {
            backend: match self.ner.backend {
                NerBackend::Onnx => kreuzberg::core::config::ner::NerBackendKind::Onnx,
                NerBackend::Llm => kreuzberg::core::config::ner::NerBackendKind::Llm,
            },
            categories: self
                .ner
                .categories
                .iter()
                .map(|s| kreuzberg::types::entity::EntityCategory::from(s.clone()))
                .collect(),
            model: self.ner.model.clone(),
            llm,
            custom_labels: self.ner.custom_labels.clone(),
        })
    }
}

/// Run kreuzberg against `path` and translate the result into a `FileMapDoc`.
///
/// `mime_type` may be supplied by the caller (e.g. from `lang::detect`); when
/// `None`, kreuzberg sniffs the file content.
pub fn extract_doc(
    path: &Path,
    mime_type: Option<&str>,
    config: &DocConfig,
) -> Result<FileMapDoc, ExtractError> {
    let krz_config = config.to_kreuzberg();
    let result = extract_file_sync(path, mime_type, &krz_config)
        .map_err(|e| ExtractError::Document(e.to_string()))?;

    let mut chunks: Vec<DocChunk> = Vec::new();
    let mut embedding_dim: u16 = 0;
    if let Some(input_chunks) = result.chunks {
        for c in input_chunks {
            let dim = c.embedding.as_ref().map(|v| v.len()).unwrap_or(0);
            if dim > 0 && embedding_dim == 0 {
                embedding_dim = u16::try_from(dim).unwrap_or(u16::MAX);
            }
            chunks.push(DocChunk {
                byte_start: u32::try_from(c.metadata.byte_start).unwrap_or(u32::MAX),
                byte_end: u32::try_from(c.metadata.byte_end).unwrap_or(u32::MAX),
                text: c.content,
                embedding: c.embedding.unwrap_or_default(),
            });
        }
    }

    let embedding_model = if embedding_dim > 0 {
        config
            .embedding_preset
            .clone()
            .unwrap_or_else(|| "default".to_string())
    } else {
        String::new()
    };

    let metadata = metadata_pairs(&result.metadata);

    let keywords: Vec<DocKeyword> = result
        .extracted_keywords
        .unwrap_or_default()
        .into_iter()
        .map(|k| DocKeyword {
            text: k.text,
            score: k.score,
            algorithm: keyword_algorithm_str(&k.algorithm).to_string(),
        })
        .collect();

    let entities: Vec<DocEntity> = result
        .entities
        .unwrap_or_default()
        .into_iter()
        .map(|e| DocEntity {
            category: entity_category_str(&e.category),
            text: e.text,
            start: e.start,
            end: e.end,
            confidence: e.confidence,
        })
        .collect();

    // `SummaryStrategy` implements `Display` upstream — formatting it directly
    // produces the same lowercase tags we'd hand-translate ("extractive" /
    // "abstractive"), and stays correct if kreuzberg adds variants.
    let summary = result.summary.map(|s| DocSummary {
        text: s.text,
        strategy: s.strategy.to_string(),
        token_count: s.token_count,
    });

    Ok(FileMapDoc {
        schema_ver: SCHEMA_VER,
        mime_type: result.mime_type.into_owned(),
        content: result.content,
        metadata,
        detected_languages: result.detected_languages.unwrap_or_default(),
        chunks,
        embedding_model,
        embedding_dim,
        keywords,
        entities,
        summary,
    })
}

/// Stable lowercase tag for kreuzberg's `KeywordAlgorithm`. We avoid `Display`
/// because the enum doesn't derive it; matching every variant keeps the
/// translation explicit and the compiler honest if kreuzberg adds variants.
fn keyword_algorithm_str(alg: &kreuzberg::KeywordAlgorithm) -> &'static str {
    match alg {
        kreuzberg::KeywordAlgorithm::Yake => "yake",
        kreuzberg::KeywordAlgorithm::Rake => "rake",
    }
}

/// Flatten kreuzberg's `EntityCategory` (a closed enum with a `Custom(String)`
/// tail variant) to a lowercase string. Standard variants use the lowercase
/// canonical name; `Custom(s)` passes the user-supplied label through verbatim.
fn entity_category_str(category: &kreuzberg::types::entity::EntityCategory) -> String {
    use kreuzberg::types::entity::EntityCategory::*;
    match category {
        Person => "person".to_string(),
        Organization => "organization".to_string(),
        Location => "location".to_string(),
        Date => "date".to_string(),
        Time => "time".to_string(),
        Money => "money".to_string(),
        Percent => "percent".to_string(),
        Email => "email".to_string(),
        Phone => "phone".to_string(),
        Url => "url".to_string(),
        Custom(s) => s.clone(),
    }
}

fn metadata_pairs(metadata: &kreuzberg::types::Metadata) -> Vec<(String, String)> {
    // Round-trip the metadata via JSON to flatten its (large, heterogeneous)
    // shape into stable string pairs without enumerating every field.
    match serde_json::to_value(metadata) {
        Ok(serde_json::Value::Object(map)) => map
            .into_iter()
            .filter_map(|(k, v)| {
                let value_str = match v {
                    serde_json::Value::Null => return None,
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                Some((k, value_str))
            })
            .collect(),
        _ => Vec::new(),
    }
}
