//! Document-tier sub-configs. Split from `v1.rs` to keep both files under the
//! 1000-line cap once iters 3–7 fill in every xberg capability.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::DeprecatedSection;

/// Top-level `[documents]` table. Each sub-config has `#[serde(default)]` so
/// adding a new tier never breaks older TOML files.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentsConfig {
    /// Master switch. Only meaningful when the `documents` cargo feature is compiled in.
    #[serde(default = "DocumentsConfig::default_enabled")]
    pub enabled: bool,
    /// MIME-type allowlist. Empty = accept anything xberg can handle.
    #[serde(default)]
    #[schemars(inner(length(min = 1)))]
    pub mime_allowlist: Vec<String>,
    /// Allow-list of globs (repo-relative, forward-slash) selecting which non-code files are indexed
    /// as documents. Empty (default) = every non-code file that passes `[scan]`. A bare name with no
    /// glob metacharacters (`docs`) matches that path and everything beneath it. `exclude` wins over
    /// `include`.
    #[serde(default)]
    #[schemars(inner(length(min = 1)))]
    pub include: Vec<String>,
    /// Deny-list of globs applied to document indexing itself (not just embedding): matching files
    /// are not extracted, chunked or searchable as documents. Same syntax as `include`. The default
    /// drops low-value bulk: `*.svg`, lockfiles (`pnpm-lock.yaml`, `package-lock.json`, `yarn.lock`,
    /// `Cargo.lock`, `poetry.lock`, `*.lock`) and test-data directories (`test_data`, `testdata`,
    /// `fixtures`, `__snapshots__`). Setting the key replaces the default list; `exclude = []`
    /// indexes all of them, and a custom list must repeat any default entry it still wants.
    #[serde(default = "DocumentsConfig::default_exclude")]
    #[schemars(inner(length(min = 1)))]
    pub exclude: Vec<String>,
    /// Per-document size cap in bytes, independent of `[scan] max_file_bytes` (which only bounds
    /// source files). Default 50 MiB, so PDFs and Office files above the 2 MiB scan cap are still
    /// extracted. Larger files are skipped and counted in the scan summary as too large.
    #[serde(default = "DocumentsConfig::default_max_file_bytes")]
    #[schemars(range(min = 1024))]
    pub max_file_bytes: u64,
    /// File extensions to skip on top of the built-in archive/binary denylist (`pdf` and `.PDF` are
    /// equivalent: case-insensitive, leading dot optional). The built-in floor
    /// (`.zip/.tar/.jar/.so/.wasm/...`) is always applied — this only adds to it — so archives and
    /// binaries are never routed to xberg extraction + embedding.
    #[serde(default)]
    #[schemars(inner(length(min = 1)))]
    pub extension_denylist: Vec<String>,
    /// Hard cap on chunks embedded per document. A pathological input (huge generated file, an
    /// archive that slipped the denylist) can otherwise explode into tens of thousands of ONNX
    /// embeds. Documents exceeding the cap are extracted but not embedded, with a warning: their
    /// chunks are indexed without vectors, so the lexical lane still finds them.
    #[serde(default = "DocumentsConfig::default_max_chunks_per_document")]
    #[schemars(range(min = 1))]
    pub max_chunks_per_document: usize,
    /// Maximum pages xberg may extract from a single document.
    #[serde(default = "DocumentsConfig::default_max_pages")]
    #[schemars(range(min = 1))]
    pub max_pages: usize,
    /// Maximum wall-clock time one document's extraction (including chunking) may take. Enforced by a
    /// watchdog: a document that overruns is abandoned, skipped and counted as `doc_timeouts` in the scan
    /// summary, so one pathological file cannot stall a scan. A timed-out document is remembered by
    /// content hash and not retried while it is unchanged and the extraction limits are unchanged;
    /// editing the file or changing this value (or the chunk cutovers or `max_pages`) retries it.
    #[serde(default = "DocumentsConfig::default_extraction_timeout_secs")]
    #[schemars(range(min = 1))]
    pub extraction_timeout_secs: u64,
    /// Wall-clock budget for the whole document phase of one scan, started at the first document that
    /// needs a real extraction. Once spent, the remaining documents are deferred (counted as
    /// `docs_deferred`, not failed) and picked up by the next scan, so several documents timing out
    /// together cost this budget rather than `extraction_timeout_secs` each. Documents already running
    /// finish or hit their own timeout, so a scan can overshoot by up to one `extraction_timeout_secs`. Never
    /// less than `extraction_timeout_secs` in effect.
    #[serde(default = "DocumentsConfig::default_phase_budget_secs")]
    #[schemars(range(min = 1))]
    pub phase_budget_secs: u64,
    /// Cap on timed-out extraction threads still running. A timed-out extraction cannot be cancelled
    /// and keeps a core busy until it finishes on its own; at the cap, further documents are deferred
    /// (counted as `docs_deferred`) instead of spawning another thread, and picked up by a later scan.
    /// A soft bound: extractions already in flight when it is reached may still time out.
    #[serde(default = "DocumentsConfig::default_max_abandoned_extractions")]
    #[schemars(range(min = 1, max = 64))]
    pub max_abandoned_extractions: usize,
    /// Largest text-like document (markdown, csv, json, yaml, xml, toml, ini, ...) in bytes still chunked
    /// with xberg's structure-aware markdown splitter. The splitter re-scans every remaining element
    /// per chunk, so its cost grows with the square of the document: a multi-megabyte file can stall a
    /// scan worker for minutes. Larger files are chunked by a linear fixed-size chunker instead (counted
    /// as `docs_degraded` in the scan summary). Never applies to binary formats (PDF, Office, ...).
    #[serde(default = "DocumentsConfig::default_markdown_chunk_max_bytes")]
    #[schemars(range(min = 1024))]
    pub markdown_chunk_max_bytes: u64,
    /// Like `markdown_chunk_max_bytes` but for plain text (`text/plain`: `.txt`, `.log`, ...), which has no
    /// markdown structure for the splitter to exploit, so the cutover is lower.
    #[serde(default = "DocumentsConfig::default_plain_text_chunk_max_bytes")]
    #[schemars(range(min = 1024))]
    pub plain_text_chunk_max_bytes: u64,
    /// Maximum chunk size in characters.
    #[serde(default = "DocumentsConfig::default_max_characters")]
    #[schemars(range(min = 64))]
    pub max_characters: usize,
    /// Overlap between chunks in characters.
    #[serde(default = "DocumentsConfig::default_overlap")]
    #[schemars(range(min = 0))]
    pub overlap: usize,
    /// Xberg embedding preset name. Defaults to "balanced".
    #[serde(default = "DocumentsConfig::default_embedding_preset")]
    pub embedding_preset: String,
    /// Generate embeddings (`true`) or skip vector storage entirely (`false`).
    #[serde(default = "DocumentsConfig::default_embed")]
    pub embed: bool,
    /// Allow-list of globs for embedding: when non-empty only matching documents are embedded; the
    /// rest stay extracted + keyword-searchable. It only narrows what `include`/`exclude` already
    /// index. Empty (default) = every indexed document outside `embed_exclude` is embedded. Only
    /// consulted when `embed = true`.
    #[serde(default)]
    #[schemars(inner(length(min = 1)))]
    pub embed_include: Vec<String>,
    /// Glob patterns (repo-relative, forward-slash) for documents that are still extracted +
    /// indexed but **never** embedded. Keeps ONNX vectors off selected large or low-value documents
    /// while leaving their text searchable by keyword. Beats `embed_include`. The default holds the
    /// structured data formats (`*.json`, `*.jsonc`, `*.json5`, `*.jsonl`, `*.ndjson`, `*.geojson`,
    /// `*.yaml`, `*.yml`, `*.toml`, `*.csv`, `*.tsv`, `*.xml`), which are searched by exact text and
    /// keyword rather than by meaning: they stay chunked, keyword-searchable and visible to
    /// `code_grep`. Setting the key
    /// replaces the default list; `embed_exclude = []` embeds them too. Only consulted when
    /// `embed = true`. Changing it removes the vector rows of newly excluded documents on the next
    /// scan.
    #[serde(default = "DocumentsConfig::default_embed_exclude")]
    #[schemars(inner(length(min = 1)))]
    pub embed_exclude: Vec<String>,
    /// Deprecated and ignored: archive extraction was removed, so archives and compressed
    /// containers (`.zip/.tar/.jar/...`) are always skipped. Accepted so existing files keep
    /// loading; `true` logs a warning.
    #[serde(default = "DocumentsConfig::default_extract_archives")]
    pub extract_archives: bool,
    /// Cap on the total *uncompressed* bytes xberg may unpack from the zip container of one Office
    /// document (`.docx/.pptx/.xlsx/.odt`). A document past the cap fails extraction (counted as an
    /// extract failure) instead of being unpacked into memory. Default 64 MiB; xberg's own default
    /// is 500 MB.
    #[serde(default = "DocumentsConfig::default_max_archive_bytes")]
    #[schemars(range(min = 1024))]
    pub max_archive_bytes: u64,
    /// Cap on the number of entries in the zip container of one Office document
    /// (`.docx/.pptx/.xlsx/.odt`). Default 1000; xberg's own default is 10,000.
    #[serde(default = "DocumentsConfig::default_max_archive_entries")]
    #[schemars(range(min = 1))]
    pub max_archive_entries: usize,
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
    /// Documentation of the repository's dependencies (ADR-0049). Off by default.
    #[serde(default)]
    pub dependencies: DependenciesConfig,
    /// Deprecated and ignored: the verb tools answer in JSON whatever this table says. Accepted so
    /// files that still carry a `[documents.output]` table keep loading; one with content logs a
    /// warning.
    #[serde(default, skip_serializing_if = "DeprecatedSection::is_empty")]
    pub output: DeprecatedSection,
    /// Maximum number of threads dedicated to ONNX embedding. `0` means
    /// "auto = max(2, logical_cpus / 4)" — a bounded fraction of available
    /// cores so the embedder never pins the machine. Both the document and
    /// code-search tiers route their `embed_texts` calls through a single
    /// process-wide rayon `ThreadPool` of this size; code-map extraction
    /// keeps the full global pool.
    #[serde(default)]
    pub embed_max_threads: usize,
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
    fn default_exclude() -> Vec<String> {
        [
            "*.svg",
            "pnpm-lock.yaml",
            "package-lock.json",
            "yarn.lock",
            "Cargo.lock",
            "poetry.lock",
            "*.lock",
            "**/test_data/**",
            "**/testdata/**",
            "**/fixtures/**",
            "**/__snapshots__/**",
        ]
        .map(String::from)
        .to_vec()
    }
    fn default_embed_exclude() -> Vec<String> {
        [
            "*.json",
            "*.jsonc",
            "*.json5",
            "*.jsonl",
            "*.ndjson",
            "*.geojson",
            "*.yaml",
            "*.yml",
            "*.toml",
            "*.csv",
            "*.tsv",
            "*.xml",
        ]
        .map(String::from)
        .to_vec()
    }
    fn default_extract_archives() -> bool {
        false
    }
    fn default_max_archive_bytes() -> u64 {
        64 * 1024 * 1024
    }
    fn default_max_archive_entries() -> usize {
        1000
    }
    fn default_max_chunks_per_document() -> usize {
        2000
    }
    fn default_max_pages() -> usize {
        500
    }
    fn default_max_file_bytes() -> u64 {
        52_428_800
    }
    fn default_extraction_timeout_secs() -> u64 {
        120
    }
    fn default_phase_budget_secs() -> u64 {
        1_800
    }
    fn default_max_abandoned_extractions() -> usize {
        4
    }
    fn default_markdown_chunk_max_bytes() -> u64 {
        1024 * 1024
    }
    fn default_plain_text_chunk_max_bytes() -> u64 {
        4 * 1024 * 1024
    }
}

impl Default for DocumentsConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            mime_allowlist: Vec::new(),
            include: Vec::new(),
            exclude: Self::default_exclude(),
            max_file_bytes: Self::default_max_file_bytes(),
            extension_denylist: Vec::new(),
            max_chunks_per_document: Self::default_max_chunks_per_document(),
            max_pages: Self::default_max_pages(),
            extraction_timeout_secs: Self::default_extraction_timeout_secs(),
            phase_budget_secs: Self::default_phase_budget_secs(),
            max_abandoned_extractions: Self::default_max_abandoned_extractions(),
            markdown_chunk_max_bytes: Self::default_markdown_chunk_max_bytes(),
            plain_text_chunk_max_bytes: Self::default_plain_text_chunk_max_bytes(),
            max_characters: Self::default_max_characters(),
            overlap: Self::default_overlap(),
            embedding_preset: Self::default_embedding_preset(),
            embed: Self::default_embed(),
            embed_include: Vec::new(),
            embed_exclude: Self::default_embed_exclude(),
            extract_archives: Self::default_extract_archives(),
            max_archive_bytes: Self::default_max_archive_bytes(),
            max_archive_entries: Self::default_max_archive_entries(),
            language: DocLanguageConfig::default(),
            reranker: RerankerConfig::default(),
            keywords: KeywordsConfig::default(),
            ner: NerConfig::default(),
            summarization: SummarizationConfig::default(),
            ocr: OcrConfig::default(),
            dependencies: DependenciesConfig::default(),
            output: DeprecatedSection::default(),
            embed_max_threads: 0,
        }
    }
}

/// `[documents.dependencies]`: index the README, CHANGELOG and `docs/**` of the repository's
/// dependencies under `deps/<ecosystem>/<name>@<version>/<file>`, so `docs_search` can filter by
/// package with its `path` glob (ADR-0049). Ecosystems: Cargo (`Cargo.toml` + `Cargo.lock`), npm
/// (`package.json`), Python (`pyproject.toml` + `uv.lock` / `poetry.lock`) and Go (`go.mod`). A
/// package root inside the repository (`node_modules/`, `.venv/`, `vendor/`) is read when this is
/// on; one outside it (the cargo registry, the Go module cache, `$VIRTUAL_ENV`) only under the
/// operator grant `BASEMIND_ALLOW_DEPENDENCY_ROOTS`. Packages that are not installed or not granted
/// are listed under `deps_unavailable` by `index_status`. The virtual paths are matched by
/// `[documents] include` / `exclude` like any other document path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DependenciesConfig {
    /// Master switch; off by default. Needs `[documents] enabled`.
    #[serde(default)]
    pub enabled: bool,
    /// Index only the dependencies the manifests name directly; `false` adds every package the
    /// lockfiles resolve.
    #[serde(default = "DependenciesConfig::default_direct_only")]
    pub direct_only: bool,
    /// Most packages indexed per scan; the rest are listed as unavailable (`over_max_packages`).
    #[serde(default = "DependenciesConfig::default_max_packages")]
    #[schemars(range(min = 1))]
    pub max_packages: usize,
    /// Most bytes of documentation read from one package: its README, then its CHANGELOG, then
    /// `docs/**` in path order; a file that would overrun the budget is skipped.
    #[serde(default = "DependenciesConfig::default_max_bytes_per_package")]
    #[schemars(range(min = 1024))]
    pub max_bytes_per_package: u64,
}

impl DependenciesConfig {
    fn default_direct_only() -> bool {
        true
    }
    fn default_max_packages() -> usize {
        200
    }
    fn default_max_bytes_per_package() -> u64 {
        2 * 1024 * 1024
    }
}

impl Default for DependenciesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            direct_only: Self::default_direct_only(),
            max_packages: Self::default_max_packages(),
            max_bytes_per_package: Self::default_max_bytes_per_package(),
        }
    }
}

/// Language detection knobs for the document tier. Named `DocLanguageConfig` to
/// avoid colliding with the per-tree-sitter-language `LanguageConfig` (which is
/// the scanner's per-grammar override map).
///
/// Xberg drives language detection through the `whatlang` crate and reports
/// ISO 639-3 codes (three letters, e.g. `"fra"`, `"deu"`) — xberg's own
/// `ExtractionResult.detected_languages` doc-comment mislabels them as ISO
/// 639-1 in rc.10, but the wrapper at `xberg::language_detection` normalises
/// every `whatlang` enum variant to its ISO 639-3 form. The `auto_detect`,
/// `min_confidence`, and `detect_multiple` knobs map straight through to
/// xberg's `LanguageDetectionConfig`. `preferred_languages` is reserved for
/// future use — xberg rc.10 does not honor a preferred-language hint, so
/// the field is plumbed but inert today; we keep it on the schema so callers
/// can start populating it without a config break when support lands.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocLanguageConfig {
    /// Run xberg's language detector. On by default; flip off when every doc
    /// in the corpus is the same known language.
    #[serde(default = "DocLanguageConfig::default_auto_detect")]
    pub auto_detect: bool,
    /// Minimum detector confidence (0.0–1.0). Detections below this threshold
    /// are dropped. Matches xberg's default of 0.8.
    #[serde(default = "DocLanguageConfig::default_min_confidence")]
    #[schemars(range(min = 0.0, max = 1.0))]
    pub min_confidence: f64,
    /// When true, xberg reports every language detected in the document
    /// instead of just the top match. Off by default to match xberg.
    #[serde(default)]
    pub detect_multiple: bool,
    /// Reserved: currently has no effect (a load-time warning is logged when set) — accepts ISO 639-3 codes (e.g. `"fra"`, `"deu"`) for future use.
    /// Xberg rc.10 does not honor a preferred-language hint, but the field
    /// is kept on the schema so users can populate it without a config break.
    #[serde(default)]
    pub preferred_languages: Vec<String>,
}

impl DocLanguageConfig {
    fn default_auto_detect() -> bool {
        true
    }
    fn default_min_confidence() -> f64 {
        0.8
    }
}

impl Default for DocLanguageConfig {
    fn default() -> Self {
        Self {
            auto_detect: Self::default_auto_detect(),
            min_confidence: Self::default_min_confidence(),
            detect_multiple: false,
            preferred_languages: Vec::new(),
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
    /// Xberg reranker preset name (`bge-reranker-base` is the default; `bge-reranker-v2-m3`,
    /// `jina-reranker-v1-turbo-en`, `ettin-reranker-150m` and `qwen3-reranker-0.6b` are the others).
    #[serde(default = "RerankerConfig::default_preset")]
    pub preset: String,
    /// How many of the best fused hits the cross-encoder reorders, at most 50. The hits after
    /// them follow in fused order: nothing is dropped.
    #[serde(default = "RerankerConfig::default_top_k")]
    #[schemars(range(min = 1))]
    pub top_k: usize,
}

impl RerankerConfig {
    fn default_preset() -> String {
        "bge-reranker-base".to_string()
    }
    fn default_top_k() -> usize {
        50
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
    /// Maps to `Some(KeywordConfig)` / `None` on `ExtractionConfig.keywords`;
    /// xberg's own `KeywordConfig` has no `enabled` field — gating is via
    /// the wrapping `Option`.
    #[serde(default)]
    pub enabled: bool,
    /// Algorithm: YAKE (statistical, multi-language) or RAKE (rapid automatic
    /// keyword extraction).
    #[serde(default)]
    pub algorithm: KeywordAlgorithm,
    /// Maximum keywords to extract per document. Matches xberg's
    /// `KeywordConfig.max_keywords` default of 10.
    #[serde(default = "KeywordsConfig::default_max_keywords")]
    #[schemars(range(min = 1))]
    pub max_keywords: usize,
    /// Minimum score threshold. Matches xberg's `KeywordConfig.min_score`
    /// default of 0.0 (i.e. surface every candidate). Score ranges differ
    /// between YAKE (lower = better) and RAKE (higher = better) — see
    /// `xberg::keywords::config::KeywordConfig.min_score`.
    #[serde(default)]
    #[schemars(range(min = 0.0))]
    pub min_score: f32,
    /// N-gram range as `[min, max]`. Matches xberg's
    /// `KeywordConfig.ngram_range` default of `(1, 3)`. Encoded as an array of
    /// length 2 so the JSON Schema stays human-readable; values map back to a
    /// `(usize, usize)` tuple at the boundary.
    #[serde(default = "KeywordsConfig::default_ngram_range")]
    #[schemars(length(min = 2, max = 2))]
    pub ngram_range: Vec<usize>,
    /// Optional YAKE tuning (passed through to xberg unchanged). Shape
    /// matches `xberg::keywords::YakeParams`; bad JSON is logged and
    /// xberg's defaults are used instead of failing the scan.
    #[serde(default)]
    pub yake_params: Option<serde_json::Value>,
    /// Optional RAKE tuning (passed through to xberg unchanged). Shape
    /// matches `xberg::keywords::RakeParams`; bad JSON is logged and
    /// xberg's defaults are used instead of failing the scan.
    #[serde(default)]
    pub rake_params: Option<serde_json::Value>,
}

impl KeywordsConfig {
    fn default_max_keywords() -> usize {
        10
    }
    fn default_ngram_range() -> Vec<usize> {
        vec![1, 3]
    }
}

impl Default for KeywordsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            algorithm: KeywordAlgorithm::default(),
            max_keywords: Self::default_max_keywords(),
            min_score: 0.0,
            ngram_range: Self::default_ngram_range(),
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
    /// Master switch — off by default; the ONNX backend downloads gline-rs weights on first use.
    /// Needs a build with the `documents-ner` Cargo feature (the published binaries have it);
    /// without it the switch has no effect and a warning is logged.
    #[serde(default)]
    pub enabled: bool,
    /// Backend selection. `onnx` uses gline-rs locally. `llm` is accepted but not supported: no
    /// entities are extracted and a warning is logged.
    #[serde(default)]
    pub backend: NerBackend,
    /// Override the ONNX model name (xberg has a default catalogue when unset).
    #[serde(default)]
    pub model: Option<String>,
    /// Categories to surface — matches xberg's `NerConfig.categories`.
    /// Accepted values are the lowercase forms `"person"`, `"organization"`,
    /// `"location"`, `"date"`, `"time"`, `"money"`, `"percent"`, `"email"`,
    /// `"phone"`, `"url"`; anything else becomes a `Custom(_)` category at the
    /// boundary. Empty means "use the backend default".
    #[serde(default)]
    pub categories: Vec<String>,
    /// Arbitrary user-supplied entity labels for gline-rs zero-shot inference. Matches xberg's
    /// `NerConfig.custom_labels`. Useful for domain-specific types like
    /// `"Treatment"`, `"Vessel"` without forking GLiNER's taxonomy. Custom
    /// labels surface as `EntityCategory::Custom(_)` in the entity stream.
    #[serde(default)]
    pub custom_labels: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum NerBackend {
    #[default]
    Onnx,
    Llm,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SummarizationConfig {
    /// Master switch — off by default. When true, the boundary code populates
    /// `ExtractionConfig.summarization`; the resulting `ExtractionResult.summary`
    /// surfaces on `FileMapDoc.summary` and `DocumentSearchHit.summary`.
    #[serde(default)]
    pub enabled: bool,
    /// Strategy: `extractive` (TextRank). `abstractive` is deprecated: LLM summarization was
    /// removed, so it logs a warning and produces extractive summaries.
    #[serde(default)]
    pub strategy: SummarizationStrategy,
    /// Soft cap on summary length in tokens. `None` lets xberg pick a default
    /// suited to the chosen strategy (the xberg-side type uses the same
    /// `Option<u32>` shape; pass-through with no policy of our own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
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
    /// Reserved: parsed but currently has no effect (a load-time warning is logged when set to a
    /// non-default value); OCR runs with xberg's defaults.
    #[serde(default)]
    pub backend: OcrBackend,
    /// Reserved: parsed but currently has no effect (a load-time warning is logged when set to a
    /// non-default value).
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

/// Deprecated and ignored: LLM credentials + model selection for the removed LLM summarization
/// and LLM entity recognition. Nothing reads it; it stays typed so existing files keep loading
/// under the strict schema and a literal `api_key` keeps serialising as `"<redacted>"`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
#[schemars(extend("deprecated" = true))]
pub struct LlmConfig {
    /// Combined provider/model string following liter-llm's routing format:
    /// `"openai/gpt-4o"`, `"anthropic/claude-sonnet-4-20250514"`,
    /// `"groq/llama-3.1-70b-versatile"`. Empty string ⇒ inert; every LLM-backed
    /// feature treats an empty `model` as "no LLM configured".
    #[serde(default)]
    pub model: String,
    /// API key — either a literal (discouraged; keeps secrets in version control)
    /// or an `{ env = "OPENAI_API_KEY" }` reference. `Unset` lets the underlying
    /// provider SDK fall back to its standard environment variable lookup. Never resolved: nothing
    /// reads `[llm]` any more.
    #[serde(default)]
    pub api_key: ApiKey,
    /// Override the provider base URL (for self-hosted vLLM, Azure OpenAI, …). Never contacted:
    /// nothing reads `[llm]` any more.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Sampling temperature. Provider-default when unset.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Request timeout in seconds. Maps to xberg's `timeout_secs` (default 60).
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub timeout_secs: Option<u64>,
    /// Maximum retry attempts on transient errors. Maps to xberg's
    /// `max_retries` (default 3).
    #[serde(default)]
    pub max_retries: Option<u32>,
    /// Maximum tokens to generate. Provider-default when unset.
    #[serde(default)]
    pub max_tokens: Option<u64>,
}

/// Tri-state API key wrapper. Serde-deserialised as `null` (`Unset`), a plain
/// string (`Literal`), or a `{ env = "NAME" }` table (`Env`).
///
/// `Serialize` is implemented manually — the `Literal` variant NEVER emits its
/// cleartext secret. It serialises to the marker string `"<redacted>"` so any
/// downstream pipeline that round-trips this enum through JSON / TOML (e.g.
/// the config validator at `src/config/mod.rs`) cannot leak credentials into
/// logs, snapshot tests, or error messages. Deserialisation is unchanged:
/// loading a TOML config with a literal `api_key = "sk-..."` still produces
/// `ApiKey::Literal(...)`. The redaction is one-way by design.
#[derive(Debug, Clone, Deserialize, JsonSchema, Default)]
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

impl PartialEq for ApiKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Literal(a), Self::Literal(b)) => a == b,
            (Self::Env { env: a }, Self::Env { env: b }) => a == b,
            (Self::Unset, Self::Unset) => true,
            _ => false,
        }
    }
}

impl serde::Serialize for ApiKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Literal(_) => serializer.serialize_str("<redacted>"),
            Self::Env { env } => {
                use serde::ser::SerializeStruct;
                let mut s = serializer.serialize_struct("EnvRef", 1)?;
                s.serialize_field("env", env)?;
                s.end()
            }
            Self::Unset => serializer.serialize_none(),
        }
    }
}

impl ApiKey {
    /// Resolve the credential. `Literal` returns its inner value as-is; `Env`
    /// performs an `env::var` lookup and returns `None` if the variable is
    /// missing or empty; `Unset` always returns `None`.
    pub fn resolve(&self) -> Option<SecretString> {
        self.resolve_with(|name| std::env::var(name).ok())
    }

    /// [`Self::resolve`] with `lookup` reading the `Env` variable in place of the process
    /// environment.
    fn resolve_with(&self, lookup: impl FnOnce(&str) -> Option<String>) -> Option<SecretString> {
        match self {
            Self::Literal(s) if !s.is_empty() => Some(SecretString(s.clone())),
            Self::Literal(_) => None,
            Self::Env { env } => lookup(env).filter(|v| !v.is_empty()).map(SecretString),
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
    fn embed_exclude_defaults_to_the_structured_data_formats() {
        let expected = [
            "*.json",
            "*.jsonc",
            "*.json5",
            "*.jsonl",
            "*.ndjson",
            "*.geojson",
            "*.yaml",
            "*.yml",
            "*.toml",
            "*.csv",
            "*.tsv",
            "*.xml",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(DocumentsConfig::default().embed_exclude, expected);
        let omitted: DocumentsConfig = toml::from_str("embed = true\n").expect("parse");
        assert_eq!(omitted.embed_exclude, expected, "an omitted key takes the default");
        let opted_in: DocumentsConfig = toml::from_str("embed_exclude = []\n").expect("parse");
        assert!(opted_in.embed_exclude.is_empty(), "an empty list replaces the default");
    }

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
    fn api_key_env_reads_the_named_variable() {
        let k = ApiKey::Env {
            env: "BASEMIND_TEST_API_KEY".to_string(),
        };
        let resolved = k
            .resolve_with(|name| (name == "BASEMIND_TEST_API_KEY").then(|| "value-123".to_string()))
            .expect("env resolves");
        assert_eq!(resolved.expose(), "value-123");
    }

    #[test]
    fn api_key_env_missing_or_empty_resolves_to_none() {
        let k = ApiKey::Env {
            env: "BASEMIND_TEST_API_KEY".to_string(),
        };
        assert!(k.resolve_with(|_| None).is_none());
        assert!(k.resolve_with(|_| Some(String::new())).is_none());
    }

    #[test]
    fn api_key_resolve_reads_the_process_environment() {
        let k = ApiKey::Env {
            env: "PATH".to_string(),
        };
        let expected = std::env::var("PATH").ok().filter(|value| !value.is_empty());
        assert_eq!(k.resolve().map(|secret| secret.expose().to_string()), expected);
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

    #[test]
    fn api_key_literal_never_serializes_cleartext() {
        let key = ApiKey::Literal("sk-supersecret".to_string());
        let json = serde_json::to_string(&key).expect("serialize");
        assert!(!json.contains("sk-supersecret"), "raw secret leaked: {json}");
        assert!(json.contains("<redacted>"), "redaction marker missing: {json}");
    }

    #[test]
    fn api_key_env_serializes_env_name_only() {
        let key = ApiKey::Env {
            env: "OPENAI_API_KEY".to_string(),
        };
        let json = serde_json::to_string(&key).expect("serialize");
        assert!(json.contains("OPENAI_API_KEY"), "env name missing: {json}");
        assert!(json.contains("\"env\""), "env field missing: {json}");
    }

    #[test]
    fn api_key_unset_serializes_to_null() {
        let key = ApiKey::Unset;
        let json = serde_json::to_string(&key).expect("serialize");
        assert_eq!(json, "null");
    }
}
