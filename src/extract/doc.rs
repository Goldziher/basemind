//! Document extraction tier — non-source files (PDFs, Office docs, emails,
//! images, …) ingested via `xberg::extract` and serialised to
//! `.basemind/blobs/<hash>.doc.msgpack`.
//!
//! Layered on top of the existing `l1` / `l2` blob shape:
//! - `l1`/`l2`/`l3` cover source code (tree-sitter outlines + calls + body hashes)
//! - `doc` covers everything else (PDFs, DOCX, XLSX, EML, HTML, images via OCR, …)
//!
//! When the document feature is on, each extracted chunk carries its embedding
//! vector inline so the scanner can stage it for LanceDB insert without a second
//! pass through the embedding engine.

use std::borrow::Cow;
use std::path::Path;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use xberg::LanguageDetectionConfig;
use xberg::core::config::processing::{ChunkingConfig, EmbeddingConfig};
use xberg::core::config::{ConcurrencyConfig, ExtractionConfig};
use xberg::{ExtractInput, extract};

use super::{ExtractError, SCHEMA_VER};
use crate::config::{
    DocLanguageConfig, DocumentModelProfile, KeywordAlgorithm, KeywordsConfig, LlmConfig, NerBackend, NerConfig,
    SummarizationConfig, SummarizationStrategy,
};

/// Per-file document extraction result. Mirrors the shape of `FileMapL1` —
/// `schema_ver` for migration, plus the structured xberg output we care
/// about for downstream vector search.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileMapDoc {
    pub schema_ver: u16,
    /// IANA MIME type as reported by xberg's detector.
    pub mime_type: String,
    /// Plain-text representation of the document (concatenation of all chunks
    /// before chunking is applied; not exactly the source bytes).
    pub content: String,
    /// Document-level metadata (author, title, dates, format-specific keys).
    /// Flattened to `String -> String` so the on-disk shape stays stable.
    pub metadata: Vec<(String, String)>,
    /// ISO 639-3 language codes detected in the content, when language
    /// detection succeeded. (Xberg's wrapper around `whatlang` normalises
    /// every detected variant to its three-letter ISO 639-3 code — see
    /// `xberg::language_detection::lang_to_iso639_3`.)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detected_languages: Vec<String>,
    /// Chunks, each with its embedding vector inline. Empty when chunking is
    /// disabled in the xberg config; embedding fields empty when the
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
    /// `None` when summarisation was disabled at scan time or when xberg
    /// declined to produce one (e.g. empty content, abstractive strategy with
    /// no LLM model configured).
    ///
    /// TAIL field — pre-iter-7 blobs deserialise via `#[serde(default)]` and
    /// surface as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<DocSummary>,
}

/// Mirror of `xberg::keywords::Keyword`, narrowed to the fields we persist.
/// We do not re-export xberg's `Keyword` directly because we control the
/// on-disk blob shape and want a forward-compatible string for `algorithm`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocKeyword {
    /// Verbatim keyword span.
    pub text: String,
    /// Backend-reported score. YAKE scores lower-is-better; RAKE higher-is-better.
    pub score: f32,
    /// `"yake"` or `"rake"` — the xberg `KeywordAlgorithm` variant stringified
    /// so consumers don't need to depend on the xberg enum.
    pub algorithm: String,
}

/// Mirror of `xberg::types::entity::Entity` with `EntityCategory` flattened
/// to a string. Flattening keeps the blob shape forward-compatible: xberg
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

/// Mirror of `xberg::DocumentSummary` with `SummaryStrategy` flattened to
/// a string. Flattening keeps the blob shape forward-compatible: xberg can
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
/// Kept independent from xberg's full `ExtractionConfig` so the scanner
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
    /// Bounded thread cap for xberg's internal ONNX embedding fan-out.
    /// `0` resolves to `max(2, cores / 4)` via `crate::embeddings::resolve_embed_threads`.
    pub embed_max_threads: usize,
    /// Chunks embedded per ONNX call (from `[resources].embed_batch_size`). Threaded into the
    /// xberg `EmbeddingConfig` so the document tier honours the same batch cap as the code-search
    /// and query embed paths.
    pub embed_batch_size: usize,
    /// Which model families run during extraction (from `[resources].document_models`). Narrower
    /// profiles strip enrichment / embeddings to shrink the scan-time footprint — see
    /// [`crate::config::DocumentModelProfile`] and [`DocConfig::to_xberg`].
    pub document_models: DocumentModelProfile,
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
            embed_max_threads: 0,
            embed_batch_size: 32,
            document_models: DocumentModelProfile::default(),
        }
    }
}

impl DocConfig {
    fn to_xberg(&self) -> ExtractionConfig {
        let no_models = matches!(self.document_models, DocumentModelProfile::None_);
        let strip_enrichment = matches!(
            self.document_models,
            DocumentModelProfile::CodeOnly | DocumentModelProfile::None_
        );
        let embedding = if self.embed && !no_models {
            Some(EmbeddingConfig {
                batch_size: self.embed_batch_size,
                ..Default::default()
            })
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
        let language_detection = if self.language.auto_detect {
            Some(LanguageDetectionConfig {
                enabled: true,
                min_confidence: self.language.min_confidence,
                detect_multiple: self.language.detect_multiple,
            })
        } else {
            None
        };
        let keywords = if strip_enrichment { None } else { self.xberg_keywords() };
        let ner = if strip_enrichment { None } else { self.xberg_ner() };
        let summarization = if strip_enrichment {
            None
        } else {
            self.xberg_summarization()
        };
        let bounded = crate::embeddings::resolve_embed_threads(self.embed_max_threads);
        let concurrency = Some(ConcurrencyConfig {
            max_threads: Some(bounded),
        });
        ExtractionConfig {
            chunking: Some(chunking),
            language_detection,
            keywords,
            ner,
            summarization,
            disable_ocr: strip_enrichment,
            concurrency,
            ..Default::default()
        }
    }

    /// Translate the basemind-side `SummarizationConfig` into xberg's
    /// `SummarizationConfig`. Returns `None` when summarisation is gated off —
    /// xberg treats `ExtractionConfig.summarization == None` as "do not run".
    ///
    /// When `strategy = Abstractive` and `[llm].model` is empty, we fall back to
    /// `Extractive` (TextRank, no LLM) with a one-time warning. This keeps the
    /// scan completing instead of failing midway with an opaque liter-llm error
    /// the agent can't act on.
    fn xberg_summarization(&self) -> Option<xberg::SummarizationConfig> {
        if !self.summarization.enabled {
            return None;
        }
        let mut sc = xberg::SummarizationConfig {
            strategy: match self.summarization.strategy {
                SummarizationStrategy::Extractive => xberg::SummaryStrategy::Extractive,
                SummarizationStrategy::Abstractive => xberg::SummaryStrategy::Abstractive,
            },
            max_tokens: self.summarization.max_tokens,
            llm: None,
        };
        if matches!(self.summarization.strategy, SummarizationStrategy::Abstractive) {
            sc.llm = self.llm.to_xberg();
            if sc.llm.is_none() {
                tracing::warn!("summarization.strategy = abstractive but llm.model unset; falling back to extractive");
                sc.strategy = xberg::SummaryStrategy::Extractive;
            }
        }
        Some(sc)
    }

    /// Translate the basemind-side `KeywordsConfig` into xberg's
    /// `KeywordConfig`. Returns `None` when keyword extraction is gated off —
    /// xberg treats `ExtractionConfig.keywords == None` as "do not run".
    ///
    /// `yake_params` / `rake_params` are typed pass-through: bad JSON is logged
    /// and dropped (xberg defaults take over) instead of failing the scan.
    fn xberg_keywords(&self) -> Option<xberg::KeywordConfig> {
        if !self.keywords.enabled {
            return None;
        }
        let ngram = if self.keywords.ngram_range.len() == 2 {
            (self.keywords.ngram_range[0], self.keywords.ngram_range[1])
        } else {
            (1, 3)
        };
        let mut kc = xberg::KeywordConfig {
            algorithm: match self.keywords.algorithm {
                KeywordAlgorithm::Yake => xberg::KeywordAlgorithm::Yake,
                KeywordAlgorithm::Rake => xberg::KeywordAlgorithm::Rake,
            },
            max_keywords: self.keywords.max_keywords,
            min_score: self.keywords.min_score,
            ngram_range: ngram,
            language: None,
            yake_params: None,
            rake_params: None,
        };
        if let Some(v) = self.keywords.yake_params.as_ref() {
            match serde_json::from_value::<xberg::keywords::YakeParams>(v.clone()) {
                Ok(p) => kc.yake_params = Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "invalid yake_params; using xberg defaults")
                }
            }
        }
        if let Some(v) = self.keywords.rake_params.as_ref() {
            match serde_json::from_value::<xberg::keywords::RakeParams>(v.clone()) {
                Ok(p) => kc.rake_params = Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "invalid rake_params; using xberg defaults")
                }
            }
        }
        if self.keywords.yake_params.is_some() && self.keywords.algorithm != KeywordAlgorithm::Yake {
            tracing::warn!(
                algorithm = ?self.keywords.algorithm,
                "yake_params set but algorithm is not Yake; params ignored"
            );
        }
        if self.keywords.rake_params.is_some() && self.keywords.algorithm != KeywordAlgorithm::Rake {
            tracing::warn!(
                algorithm = ?self.keywords.algorithm,
                "rake_params set but algorithm is not Rake; params ignored"
            );
        }
        Some(kc)
    }

    /// Translate the basemind-side `NerConfig` into xberg's
    /// `core::config::NerConfig`. `None` when NER is gated off.
    ///
    /// String category names round-trip via `EntityCategory::from(String)` —
    /// unknown names land in the `Custom(_)` variant rather than failing.
    ///
    /// When `backend == Llm`, the shared `LlmConfig` is resolved via
    /// `to_xberg()` and threaded into the xberg-side `NerConfig.llm`.
    /// If the user selected the LLM backend but left `llm.model` empty, we
    /// emit a warning — xberg silently falls back to ONNX in that case
    /// and the user almost certainly wants to know.
    fn xberg_ner(&self) -> Option<xberg::core::config::ner::NerConfig> {
        if !self.ner.enabled {
            return None;
        }
        let llm = if matches!(self.ner.backend, NerBackend::Llm) {
            let cfg = self.llm.to_xberg();
            if cfg.is_none() {
                tracing::warn!("ner.backend = llm but llm.model is unset; NER will fall back to ONNX inside xberg");
            }
            cfg
        } else {
            None
        };
        Some(xberg::core::config::ner::NerConfig {
            backend: match self.ner.backend {
                NerBackend::Onnx => xberg::core::config::ner::NerBackendKind::Onnx,
                NerBackend::Llm => xberg::core::config::ner::NerBackendKind::Llm,
            },
            categories: self
                .ner
                .categories
                .iter()
                .map(|s| xberg::types::entity::EntityCategory::from(s.clone()))
                .collect(),
            model: self.ner.model.clone(),
            llm,
            custom_labels: self.ner.custom_labels.clone(),
        })
    }
}

/// Shared multi-thread Tokio runtime for driving xberg's async extraction API
/// from the synchronous rayon scan path.
///
/// xberg 1.0 dropped its `extract_file_sync` wrapper — `extract` is async-only,
/// so basemind owns the sync bridge. Built once and never dropped; rayon workers
/// `block_on` it concurrently (each future is driven to completion on the shared
/// worker pool).
fn extraction_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build xberg extraction runtime")
    })
}

/// Run xberg against `path` and translate the result into a `FileMapDoc`.
///
/// `mime_type` may be supplied by the caller (e.g. from `lang::detect`); when
/// `None`, xberg sniffs the file content.
pub fn extract_doc(path: &Path, mime_type: Option<&str>, config: &DocConfig) -> Result<FileMapDoc, ExtractError> {
    let krz_config = config.to_xberg();
    let mut input = ExtractInput::from_uri(path.to_string_lossy().into_owned());
    input.mime_type = mime_type.map(str::to_string);
    let mut extraction = extraction_runtime()
        .block_on(extract(input, &krz_config))
        .map_err(|e| ExtractError::Document(e.to_string()))?;
    let result = extraction.results.pop().ok_or_else(|| {
        let message = extraction
            .errors
            .into_iter()
            .next()
            .map(|e| e.message)
            .unwrap_or_else(|| "xberg returned no extracted document".to_string());
        ExtractError::Document(message)
    })?;

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
        config.embedding_preset.clone().unwrap_or_else(|| "default".to_string())
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
            category: entity_category_str(e.category).into_owned(),
            text: e.text,
            start: e.start,
            end: e.end,
            confidence: e.confidence,
        })
        .collect();

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

/// Stable lowercase tag for xberg's `KeywordAlgorithm`. We avoid `Display`
/// because the enum doesn't derive it; matching every variant keeps the
/// translation explicit and the compiler honest if xberg adds variants.
fn keyword_algorithm_str(alg: &xberg::KeywordAlgorithm) -> &'static str {
    match alg {
        xberg::KeywordAlgorithm::Yake => "yake",
        xberg::KeywordAlgorithm::Rake => "rake",
    }
}

/// Flatten xberg's `EntityCategory` (a closed enum with a `Custom(String)`
/// tail variant) to a lowercase string. Standard variants return a `'static`
/// borrow — zero allocation. `Custom(s)` moves `s` into a `Cow::Owned` so
/// callers can call `.into_owned()` without an extra clone for the common case.
fn entity_category_str(category: xberg::types::entity::EntityCategory) -> Cow<'static, str> {
    use xberg::types::entity::EntityCategory::*;
    match category {
        Person => Cow::Borrowed("person"),
        Organization => Cow::Borrowed("organization"),
        Location => Cow::Borrowed("location"),
        Date => Cow::Borrowed("date"),
        Time => Cow::Borrowed("time"),
        Money => Cow::Borrowed("money"),
        Percent => Cow::Borrowed("percent"),
        Email => Cow::Borrowed("email"),
        Phone => Cow::Borrowed("phone"),
        Url => Cow::Borrowed("url"),
        Custom(s) => Cow::Owned(s),
    }
}

fn metadata_pairs(metadata: &xberg::types::Metadata) -> Vec<(String, String)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `DocConfig` with every enrichment post-processor turned on so a
    /// profile that strips them is observable in `to_xberg`'s output.
    fn doc_config_all_enrichment_on(profile: DocumentModelProfile) -> DocConfig {
        DocConfig {
            keywords: KeywordsConfig {
                enabled: true,
                ..KeywordsConfig::default()
            },
            ner: NerConfig {
                enabled: true,
                ..NerConfig::default()
            },
            summarization: SummarizationConfig {
                enabled: true,
                ..SummarizationConfig::default()
            },
            document_models: profile,
            ..DocConfig::default()
        }
    }

    #[test]
    fn to_xberg_full_profile_keeps_enrichment_and_ocr() {
        let cfg = doc_config_all_enrichment_on(DocumentModelProfile::Full);
        let x = cfg.to_xberg();
        assert!(x.keywords.is_some(), "Full keeps keyword extraction");
        assert!(x.ner.is_some(), "Full keeps NER");
        assert!(x.summarization.is_some(), "Full keeps summarisation");
        assert!(!x.disable_ocr, "Full leaves OCR enabled");
        assert!(
            x.chunking.as_ref().and_then(|c| c.embedding.as_ref()).is_some(),
            "Full embeds when embed = true"
        );
    }

    #[test]
    fn to_xberg_code_only_strips_enrichment_disables_ocr_keeps_embeddings() {
        let cfg = doc_config_all_enrichment_on(DocumentModelProfile::CodeOnly);
        let x = cfg.to_xberg();
        assert!(x.keywords.is_none(), "CodeOnly forces keywords off");
        assert!(x.ner.is_none(), "CodeOnly forces NER off");
        assert!(x.summarization.is_none(), "CodeOnly forces summarisation off");
        assert!(x.disable_ocr, "CodeOnly disables OCR");
        assert!(
            x.chunking.as_ref().and_then(|c| c.embedding.as_ref()).is_some(),
            "CodeOnly still embeds (embed = true)"
        );
    }

    #[test]
    fn to_xberg_none_profile_strips_everything_including_embeddings() {
        let cfg = doc_config_all_enrichment_on(DocumentModelProfile::None_);
        let x = cfg.to_xberg();
        assert!(x.keywords.is_none(), "None strips keywords");
        assert!(x.ner.is_none(), "None strips NER");
        assert!(x.summarization.is_none(), "None strips summarisation");
        assert!(x.disable_ocr, "None disables OCR");
        assert!(
            x.chunking.as_ref().and_then(|c| c.embedding.as_ref()).is_none(),
            "None skips embeddings entirely even when embed = true"
        );
    }

    #[test]
    fn to_xberg_threads_embed_batch_size_into_embedding_config() {
        let cfg = DocConfig {
            embed: true,
            embed_batch_size: 8,
            ..DocConfig::default()
        };
        let x = cfg.to_xberg();
        let batch = x
            .chunking
            .as_ref()
            .and_then(|c| c.embedding.as_ref())
            .map(|e| e.batch_size);
        assert_eq!(batch, Some(8), "embed_batch_size flows into EmbeddingConfig.batch_size");
    }
}
