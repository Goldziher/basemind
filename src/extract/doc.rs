//! Document extraction tier — non-source files (PDFs, Office docs, emails,
//! images, …) ingested via `xberg::extract` and serialised to
//! `.basemind/blobs/<hash>.doc.msgpack`.
//!
//! Layered on top of the existing `l1` / `l2` blob shape:
//! - `l1`/`l2`/`l3` cover source code (tree-sitter outlines + calls + body hashes)
//! - `doc` covers everything else (PDFs, DOCX, XLSX, EML, HTML, …; images and scanned PDFs when the
//!   OCR tier is compiled in, see [`super::doc_tiers`])
//!
//! When the document feature is on, each extracted chunk carries its embedding
//! vector inline so the scanner can stage it for LanceDB insert without a second
//! pass through the embedding engine.

use std::borrow::Cow;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use xberg::LanguageDetectionConfig;
use xberg::core::config::processing::{ChunkerType, ChunkingConfig};
use xberg::core::config::{ConcurrencyConfig, ExtractionConfig};
use xberg::extractors::security::SecurityLimits;
use xberg::{ExtractInput, extract};

use super::doc_guard::{self, ChunkCutovers, DeadlineError};
use super::{ExtractError, SCHEMA_VER};
use crate::backpressure::DocPhase;
use crate::config::{
    DocLanguageConfig, DocumentModelProfile, KeywordAlgorithm, KeywordsConfig, NerBackend, NerConfig, OnnxProvider,
    SummarizationConfig,
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
    /// declined to produce one (e.g. empty content).
    ///
    /// TAIL field — pre-iter-7 blobs deserialise via `#[serde(default)]` and
    /// surface as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<DocSummary>,
    /// Structured language detection results reported by xberg 1.1.
    ///
    /// Tail field so older positional msgpack blobs remain readable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub language_confidences: Vec<DocLanguageConfidence>,
    /// Fingerprint ([`crate::config::rules::doc_digest`]) of the extraction settings this document
    /// was produced under; a blob under a different digest is re-extracted. Stamped by the scanner,
    /// empty straight out of `extract_doc` and on blobs that predate the field (adopted on first
    /// reuse). Tail field for the same reason as `language_confidences`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub config_digest: String,
    /// True when the chunks came from the linear fixed-size chunker instead of xberg's markdown
    /// splitter (large text-like files; see [`super::doc_guard`]). Tail field, absent in older blobs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub linear_chunked: bool,
    /// True when xberg's structured extractor rejected the file (parse error or security limit) and
    /// it was re-read as `text/plain` instead; implies `linear_chunked`. Tail field, absent in older
    /// blobs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub text_fallback: bool,
    /// True when vectors were asked for and the embedder could not produce them: the document owes
    /// its vectors and is re-extracted once the embedder works. Tail field, absent in older blobs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub embed_failed: bool,
}

#[path = "doc_blobs.rs"]
mod blobs;
pub use blobs::DocBlobs;

/// Stable mirror of xberg's per-language confidence metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct DocLanguageConfidence {
    /// ISO 639-3 language code.
    pub language: String,
    /// Detector confidence in `[0.0, 1.0]`.
    pub confidence: f64,
    /// Share of analyzed document content attributed to this language.
    pub proportion: f64,
    /// Detected writing system, such as `Latin` or `Cyrillic`.
    pub script: String,
    /// Whether xberg considers this detection reliable.
    pub reliable: bool,
}

/// Mirror of `xberg::keywords::Keyword`, narrowed to the fields we persist.
/// We do not re-export xberg's `Keyword` directly because we control the
/// on-disk blob shape and want a forward-compatible string for `algorithm`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct DocSummary {
    /// Plain-prose summary text.
    pub text: String,
    /// Strategy that produced this summary: `"extractive"` (TextRank). Blobs written before LLM
    /// summarization was removed may still say `"abstractive"`.
    pub strategy: String,
    /// Approximate token count of `text`, when the backend reports one. `None`
    /// when the backend (typically the extractive path) does not measure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_count: Option<u32>,
}

/// A single chunked region of a document.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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
    /// xberg's classification of the chunk (`heading`, `table_like`, `code_block`, ...), snake
    /// case; empty for the linear chunker. Tail fields from here on: older blobs decode them empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// Pages (slides) the chunk spans, 1-based; paged formats only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_start: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_end: Option<u32>,
    /// Heading trail the chunk sits under, outermost first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub section: Vec<String>,
    /// Source lines the chunk came from, 1-based; text sources only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_start: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_end: Option<u32>,
}

#[path = "doc_text.rs"]
pub(crate) mod text;

/// Caller-supplied knobs for document extraction.
///
/// Kept independent from xberg's full `ExtractionConfig` so the scanner
/// callsite stays readable; we translate to `ExtractionConfig` at the boundary.
#[derive(Debug, Clone)]
pub struct DocConfig {
    pub max_pages: usize,
    pub extraction_timeout_secs: u64,
    /// Cap on timed-out extraction threads still running (`[documents] max_abandoned_extractions`).
    /// At the cap, further extractions are deferred instead of spawning another thread.
    pub max_abandoned_extractions: usize,
    /// Whole-scan document-phase budget, shared by every document of one scan. `None` (direct callers,
    /// tests) means unbounded.
    pub phase: Option<Arc<DocPhase>>,
    /// Size cutovers above which text-like files skip xberg's quadratic markdown splitter.
    pub chunk_cutovers: ChunkCutovers,
    /// Chunk-count cap (`[documents] max_chunks_per_document`). Beyond it no vector rows are written, so
    /// a linearly-chunked document over the cap skips embedding entirely rather than embedding chunks
    /// that would be thrown away.
    pub max_chunks_per_document: usize,
    pub max_characters: usize,
    pub overlap: usize,
    pub embedding_preset: Option<String>,
    pub embed: bool,
    pub language: DocLanguageConfig,
    pub keywords: KeywordsConfig,
    pub ner: NerConfig,
    /// Summarisation knobs (`enabled`, `max_tokens`; `strategy` is always extractive).
    pub summarization: SummarizationConfig,
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
    /// ONNX execution provider for the embedder and xberg's NER model (from
    /// `[resources].onnx_provider`).
    pub onnx_provider: OnnxProvider,
    /// Cap on total uncompressed bytes unpacked from one archive (from `[documents].max_archive_bytes`).
    pub max_archive_bytes: usize,
    /// Cap on entries in one archive (from `[documents].max_archive_entries`).
    pub max_archive_entries: usize,
}

impl Default for DocConfig {
    fn default() -> Self {
        Self {
            max_pages: 500,
            extraction_timeout_secs: 120,
            max_abandoned_extractions: 4,
            phase: None,
            chunk_cutovers: ChunkCutovers {
                markdown_bytes: 1024 * 1024,
                plain_text_bytes: 4 * 1024 * 1024,
            },
            max_chunks_per_document: 2000,
            max_characters: 1000,
            overlap: 200,
            embedding_preset: Some("balanced".to_string()),
            embed: true,
            language: DocLanguageConfig::default(),
            keywords: KeywordsConfig::default(),
            ner: NerConfig::default(),
            summarization: SummarizationConfig::default(),
            embed_max_threads: 0,
            embed_batch_size: 32,
            document_models: DocumentModelProfile::default(),
            onnx_provider: OnnxProvider::default(),
            max_archive_bytes: 64 * 1024 * 1024,
            max_archive_entries: 1000,
        }
    }
}

impl DocConfig {
    fn to_xberg(&self) -> ExtractionConfig {
        let strip_enrichment = matches!(
            self.document_models,
            DocumentModelProfile::CodeOnly | DocumentModelProfile::None_
        );
        // Tables repeat their header in every chunk they are split over (ADR-0029 rule 10).
        let chunking = ChunkingConfig {
            max_characters: self.max_characters,
            overlap: self.overlap,
            embedding: None,
            preset: self.embedding_preset.clone(),
            chunker_type: ChunkerType::Markdown,
            table_chunking: xberg::core::config::processing::TableChunkingMode::RepeatHeader,
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
            // Defer to `max_threads` for the OCR session count. ~keep
            max_concurrent_ocr: None,
        });
        let security_limits = SecurityLimits {
            max_pages: Some(self.max_pages),
            max_archive_size: self.max_archive_bytes,
            max_files_in_archive: self.max_archive_entries,
            ..SecurityLimits::default()
        };
        ExtractionConfig {
            use_cache: false,
            chunking: Some(chunking),
            language_detection,
            keywords,
            ner,
            summarization,
            disable_ocr: strip_enrichment || !super::doc_tiers::ocr_compiled(),
            acceleration: crate::embeddings::acceleration(self.onnx_provider),
            concurrency,
            extraction_timeout_secs: Some(self.extraction_timeout_secs),
            security_limits: Some(security_limits),
            // Markdown keeps headings, tables and fences in the content, so the splitter cuts at
            // them and PDF chunks get a section (benchmarks/retrieval/PROBES.md).
            output_format: xberg::core::config::OutputFormat::Markdown,
            ..Default::default()
        }
    }

    /// Translate the basemind-side `SummarizationConfig` into xberg's
    /// `SummarizationConfig`. Returns `None` when summarisation is gated off —
    /// xberg treats `ExtractionConfig.summarization == None` as "do not run".
    ///
    /// Always extractive (TextRank): the LLM backend is not compiled in, and a config that still
    /// says `strategy = "abstractive"` is reported once at load
    /// ([`crate::config::trust::deprecated_settings_set`]).
    fn xberg_summarization(&self) -> Option<xberg::SummarizationConfig> {
        self.summarization.enabled.then(|| xberg::SummarizationConfig {
            strategy: xberg::SummaryStrategy::Extractive,
            max_tokens: self.summarization.max_tokens,
            llm: None,
        })
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
            xberg::NgramRange {
                min: self.keywords.ngram_range[0],
                max: self.keywords.ngram_range[1],
            }
        } else {
            xberg::NgramRange { min: 1, max: 3 }
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
    /// Also `None` when NER cannot run in this build: the `documents-ner` tier is not compiled in,
    /// or the config selects the LLM backend, which basemind never ships. Both are reported by
    /// [`super::doc_tiers::warn_unavailable_settings`] rather than left to xberg, which would skip
    /// the first silently and fail every document on the second.
    fn xberg_ner(&self) -> Option<xberg::core::config::ner::NerConfig> {
        if !self.ner.enabled || !super::doc_tiers::ner_compiled() || self.ner.backend == NerBackend::Llm {
            return None;
        }
        Some(xberg::core::config::ner::NerConfig {
            backend: xberg::core::config::ner::NerBackendKind::Onnx,
            categories: self
                .ner
                .categories
                .iter()
                .map(|s| xberg::types::entity::EntityCategory::from(s.clone()))
                .collect(),
            model: self.ner.model.clone(),
            llm: None,
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
///
/// A failed build (thread exhaustion) surfaces as an [`ExtractError`] for the file at hand instead of
/// panicking a scan worker; the failure is cached, so later files fail fast the same way.
fn extraction_runtime() -> Result<&'static tokio::runtime::Runtime, ExtractError> {
    static RT: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RT.get_or_init(|| {
        // Capped: the default is one worker per core plus a 512-thread blocking pool, on top of the
        // scanner's own per-core rayon pool that drives it.
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 4)))
            .max_blocking_threads(16)
            .thread_keep_alive(std::time::Duration::from_secs(5))
            .enable_all()
            .build()
            .map_err(|e| format!("build xberg extraction runtime: {e}"))
    })
    .as_ref()
    .map_err(|message| ExtractError::Document(message.clone()))
}

/// One guarded xberg call. It runs on its own thread under the wall-clock budget, so the text/plain
/// retry in [`extract_doc`] is bounded exactly like the first attempt.
fn run_guarded(
    path: &Path,
    mime_type: Option<&str>,
    krz_config: ExtractionConfig,
    config: &DocConfig,
) -> Result<xberg::Result<xberg::ExtractionResult>, ExtractError> {
    let mut input = ExtractInput::from_uri(path.to_string_lossy().into_owned());
    input.mime_type = mime_type.map(str::to_string);
    let runtime = extraction_runtime()?;
    let budget = std::time::Duration::from_secs(config.extraction_timeout_secs);
    let ledger = doc_guard::AbandonedThreads::global();
    doc_guard::run_with_deadline(ledger, config.max_abandoned_extractions, budget, move || {
        runtime.block_on(extract(input, &krz_config))
    })
    .map_err(|error| match error {
        DeadlineError::Elapsed => {
            tracing::warn!(
                path = %path.display(),
                budget_secs = budget.as_secs(),
                abandoned_live = ledger.live(),
                abandoned_cap = config.max_abandoned_extractions,
                "document extraction exceeded [documents] extraction_timeout_secs; abandoning it"
            );
            ExtractError::DocTimeout(budget)
        }
        DeadlineError::AtCapacity { live, cap } => {
            tracing::debug!(
                path = %path.display(),
                abandoned_live = live,
                abandoned_cap = cap,
                "deferring document: abandoned extraction threads are at [documents] max_abandoned_extractions"
            );
            ExtractError::DocDeferred(format!(
                "{live} timed-out extraction threads are still running (cap {cap}, \
                 [documents] max_abandoned_extractions)"
            ))
        }
        DeadlineError::Failed(message) => ExtractError::Document(message),
    })
}

/// Admit one more document to the scan's document phase, or defer it. A deferral is logged at debug
/// only: the scan reports one summary count at the end of the phase instead of a line per file.
fn admit_to_phase(path: &Path, phase: &DocPhase) -> Result<(), ExtractError> {
    phase.admit().map_err(|spent| {
        tracing::debug!(
            path = %path.display(),
            elapsed_secs = spent.elapsed.as_secs(),
            budget_secs = spent.budget.as_secs(),
            "document-phase budget spent ([documents] phase_budget_secs); deferring the document to the next scan"
        );
        ExtractError::DocDeferred(format!(
            "the {}s document-phase budget ([documents] phase_budget_secs) is spent",
            spent.budget.as_secs()
        ))
    })
}

/// Run xberg against `path` and translate the result into a `FileMapDoc`.
///
/// `mime_type` may be supplied by the caller (e.g. from `lang::detect`); when
/// `None`, xberg sniffs the file content.
pub fn extract_doc(path: &Path, mime_type: Option<&str>, config: &DocConfig) -> Result<FileMapDoc, ExtractError> {
    let mut krz_config = config.to_xberg();
    let file_len = std::fs::metadata(path).map_or(0, |m| m.len());
    if let Some(phase) = &config.phase {
        admit_to_phase(path, phase)?;
    }
    let linear = doc_guard::prefers_linear_chunking(mime_type.unwrap_or(""), file_len, config.chunk_cutovers);
    if linear {
        tracing::debug!(
            path = %path.display(),
            bytes = file_len,
            mime = mime_type.unwrap_or(""),
            "using linear chunking instead of the markdown splitter"
        );
        krz_config.chunking = None;
    }
    let mut outcome = run_guarded(path, mime_type, krz_config.clone(), config)?;
    let mut text_fallback = false;
    if let Err(error) = &outcome
        && super::doc_fallback::should_retry_as_text(mime_type, error)
    {
        tracing::warn!(
            path = %path.display(),
            mime = mime_type.unwrap_or(""),
            error = %error,
            "structured document extraction failed; retrying as text/plain with linear chunking"
        );
        krz_config.chunking = None;
        match run_guarded(path, Some("text/plain"), krz_config, config)? {
            Ok(result) => {
                outcome = Ok(result);
                text_fallback = true;
            }
            Err(retry_error) => tracing::warn!(
                path = %path.display(),
                error = %retry_error,
                "text/plain retry also failed; reporting the original error"
            ),
        }
    }
    let linear = linear || text_fallback;
    let mut extraction = outcome.map_err(|e| ExtractError::Document(e.to_string()))?;
    let result = extraction.results.pop().ok_or_else(|| {
        let message = extraction
            .errors
            .into_iter()
            .next()
            .map(|e| e.message)
            .unwrap_or_else(|| "xberg returned no extracted document".to_string());
        ExtractError::Document(message)
    })?;
    if let Some(warning) = super::doc_tiers::textless_pdf_warning(&result.mime_type, &result.content) {
        tracing::warn!(path = %path.display(), "{warning}");
    }

    let embed_requested = config.embed && !matches!(config.document_models, DocumentModelProfile::None_);
    let inputs: Vec<text::ChunkInput<'_>> = if linear {
        doc_guard::linear_chunk_spans(&result.content, config.max_characters, config.overlap)
            .into_iter()
            .map(|(start, end)| text::ChunkInput::of_span(&result.content, start, end))
            .collect()
    } else {
        result
            .chunks
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(text::ChunkInput::of_xberg)
            .collect()
    };
    let source = text::read_text_source(path, &result.mime_type);
    let (mut chunks, mut dense_inputs) = text::build_chunks(
        &result.content,
        &result.mime_type,
        source.as_deref(),
        inputs,
        embed_requested,
    );

    if linear && chunks.len() > config.max_chunks_per_document {
        tracing::warn!(
            path = %path.display(),
            chunks = chunks.len(),
            cap = config.max_chunks_per_document,
            "linearly chunked document exceeds max_chunks_per_document; skipping embedding"
        );
        dense_inputs.clear();
    }
    // An embedder that cannot load or run leaves the document indexed without vectors: it is still
    // found by the lexical lane, and the vector lane says it has none (review M1).
    let mut embed_failed = false;
    let embedding_dim = if embed_requested && !dense_inputs.is_empty() {
        let preset = config.embedding_preset.as_deref().unwrap_or("balanced");
        let embedded = text::embed_chunks(&dense_inputs, &mut chunks, |inputs| {
            text::embed_document_inputs(
                preset,
                config.embed_max_threads,
                config.embed_batch_size,
                config.onnx_provider,
                inputs,
            )
        });
        embedded.unwrap_or_else(|reason| {
            tracing::warn!(path = %path.display(), %reason, "document indexed without vectors");
            embed_failed = true;
            0
        })
    } else {
        0
    };

    let embedding_model = if embedding_dim > 0 {
        config.embedding_preset.as_deref().map_or_else(
            || "default".to_string(),
            |p| crate::embeddings::model_key(p).into_owned(),
        )
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

    let language_confidences = map_language_confidences(result.detected_language_confidences.unwrap_or_default());

    Ok(FileMapDoc {
        config_digest: String::new(),
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
        language_confidences,
        linear_chunked: linear,
        text_fallback,
        embed_failed,
    })
}

fn map_language_confidences(input: Vec<xberg::types::LanguageConfidence>) -> Vec<DocLanguageConfidence> {
    input
        .into_iter()
        .map(|language| DocLanguageConfidence {
            language: language.language,
            confidence: language.confidence,
            proportion: language.proportion,
            script: language.script,
            reliable: language.reliable,
        })
        .collect()
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
    use xberg::types::{HeadingContext, HeadingLevel};

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
    fn to_xberg_full_profile_keeps_enrichment_and_the_compiled_tiers() {
        let cfg = doc_config_all_enrichment_on(DocumentModelProfile::Full);
        let x = cfg.to_xberg();
        assert!(x.keywords.is_some(), "Full keeps keyword extraction");
        assert_eq!(
            x.ner.is_some(),
            crate::extract::doc_tiers::ner_compiled(),
            "Full runs NER exactly when the tier is compiled in"
        );
        assert!(x.summarization.is_some(), "Full keeps summarisation");
        assert_eq!(
            x.disable_ocr,
            !crate::extract::doc_tiers::ocr_compiled(),
            "Full leaves OCR on exactly when the tier is compiled in"
        );
        assert!(x.chunking.as_ref().and_then(|c| c.embedding.as_ref()).is_none());
    }

    #[test]
    fn to_xberg_code_only_strips_enrichment_disables_ocr_keeps_embeddings() {
        let cfg = doc_config_all_enrichment_on(DocumentModelProfile::CodeOnly);
        let x = cfg.to_xberg();
        assert!(x.keywords.is_none(), "CodeOnly forces keywords off");
        assert!(x.ner.is_none(), "CodeOnly forces NER off");
        assert!(x.summarization.is_none(), "CodeOnly forces summarisation off");
        assert!(x.disable_ocr, "CodeOnly disables OCR");
        assert!(x.chunking.as_ref().and_then(|c| c.embedding.as_ref()).is_none());
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
    fn abstractive_summarization_falls_back_to_extractive() {
        let cfg = DocConfig {
            summarization: SummarizationConfig {
                enabled: true,
                strategy: crate::config::SummarizationStrategy::Abstractive,
                max_tokens: Some(64),
            },
            ..DocConfig::default()
        };
        let summarization = cfg.to_xberg().summarization.expect("summarisation stays on");
        assert_eq!(summarization.strategy, xberg::SummaryStrategy::Extractive);
        assert_eq!(summarization.max_tokens, Some(64));
        assert!(summarization.llm.is_none());
    }

    #[test]
    fn the_llm_ner_backend_is_never_handed_to_xberg() {
        let cfg = DocConfig {
            ner: NerConfig {
                enabled: true,
                backend: NerBackend::Llm,
                ..NerConfig::default()
            },
            ..DocConfig::default()
        };
        assert!(cfg.to_xberg().ner.is_none());
    }

    #[test]
    fn to_xberg_defers_embedding_to_basemind() {
        let cfg = DocConfig {
            embed: true,
            embed_batch_size: 8,
            ..DocConfig::default()
        };
        let x = cfg.to_xberg();
        assert!(
            x.chunking.as_ref().and_then(|c| c.embedding.as_ref()).is_none(),
            "basemind must embed the breadcrumbed retrieval view after extraction"
        );
    }

    #[test]
    fn to_xberg_bounds_pages_timeout_and_disables_duplicate_cache() {
        let cfg = DocConfig {
            max_pages: 37,
            extraction_timeout_secs: 42,
            ..doc_config_all_enrichment_on(DocumentModelProfile::Full)
        };

        let xberg = cfg.to_xberg();
        let security = xberg.security_limits.expect("security limits configured");
        let chunking = xberg.chunking.expect("chunking configured");

        assert!(
            !xberg.use_cache,
            "basemind's content-addressed blob cache is the only extraction cache"
        );
        assert_eq!(xberg.extraction_timeout_secs, Some(42));
        assert_eq!(security.max_pages, Some(37));
        assert_eq!(chunking.chunker_type, ChunkerType::Markdown);
    }

    fn input<'a>(text: &str, context: Option<&'a HeadingContext>) -> text::ChunkInput<'a> {
        text::ChunkInput {
            text: text.to_string(),
            byte_start: 0,
            byte_end: text.len(),
            kind: String::new(),
            pages: Some((3, 4)),
            section: vec!["Guide".to_string()],
            heading_context: context,
        }
    }

    #[test]
    fn dense_retrieval_text_adds_breadcrumb_without_changing_source() {
        let source = "## Setup\n\nInstall dependencies.";
        let context = HeadingContext {
            headings: vec![
                HeadingLevel {
                    level: 1,
                    text: "Guide".to_string(),
                },
                HeadingLevel {
                    level: 2,
                    text: "Setup".to_string(),
                },
            ],
        };

        let (stored, retrieval) =
            text::build_chunks(source, "text/markdown", None, vec![input(source, Some(&context))], true);

        assert_eq!(stored[0].text, source);
        assert_eq!((stored[0].byte_start, stored[0].byte_end), (0, 31));
        assert_eq!(stored[0].section, vec!["Guide".to_string()]);
        assert_eq!(stored[0].page_start, None, "a Markdown file has no pages");
        assert_eq!(
            retrieval,
            vec!["# Guide > ## Setup\n\nInstall dependencies.".to_string()]
        );
    }

    #[test]
    fn paged_formats_keep_their_pages_and_embedding_off_builds_no_input() {
        let source = "Exact source span";
        let (stored, retrieval) = text::build_chunks(source, "application/pdf", None, vec![input(source, None)], false);
        assert_eq!(stored[0].text, "Exact source span");
        assert_eq!((stored[0].page_start, stored[0].page_end), (Some(3), Some(4)));
        assert!(retrieval.is_empty());
    }

    #[test]
    fn language_confidences_are_persisted_without_xberg_types() {
        let mapped = map_language_confidences(vec![xberg::types::LanguageConfidence {
            language: "eng".to_string(),
            confidence: 0.98,
            proportion: 0.75,
            script: "Latin".to_string(),
            reliable: true,
        }]);

        assert_eq!(
            mapped,
            vec![DocLanguageConfidence {
                language: "eng".to_string(),
                confidence: 0.98,
                proportion: 0.75,
                script: "Latin".to_string(),
                reliable: true,
            }]
        );
    }
}
