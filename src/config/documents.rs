//! Document-tier sub-configs. Split from `v1.rs` to keep both files under the
//! 1000-line cap once iters 3–7 fill in every xberg capability.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::DeprecatedSection;
use super::secrets::ApiKey;

/// Globs naming test, fixture, mock, story, snapshot and end-to-end paths. The single source for
/// two uses that never drift: `[scan] test_globs` names the test paths (they stay indexed in the
/// code map for symbols, outlines, refs and grep), and the document tier's default `exclude`
/// drops them from RAG (document indexing and embedding).
///
/// Patterns are anchored on a directory or a suffix. `**/test_*.*` can also hit a production
/// module such as `test_runner.py`; that is accepted because a user can
/// re-include with `!pattern` in `[documents] exclude`.
pub const TEST_PATH_GLOBS: &[&str] = &[
    "**/tests/**",
    "**/test/**",
    "**/__tests__/**",
    "**/e2e/**",
    "**/mocks/**",
    "**/fixtures/**",
    "**/__snapshots__/**",
    "**/test_data/**",
    "**/testdata/**",
    "**/*_test.*",
    "**/test_*.*",
    "**/*.spec.*",
    "**/*.test.*",
    "**/conftest.py",
    "**/*.bats",
    "**/*.snap",
    "**/*.stories.*",
    "**/*.mock.*",
    "**/*.mocks.*",
    "**/__mocks__/**",
    "**/mock/**",
    "**/testing/**",
    "**/playwright/**",
    "**/cypress/**",
    "**/stories/**",
    "**/*Test.java",
    "**/*Tests.java",
    "**/*Test.kt",
    "**/*_spec.rb",
    "**/*_tests.rs",
    "**/test_utils/**",
    "**/testutils/**",
    "**/tests.py",
    "**/*.e2e.*",
    "**/*.cy.*",
];

/// Agent instruction and configuration files: they describe how agents should work, not the
/// project, so they stay out of document search.
pub const AGENT_CONFIG_GLOBS: &[&str] = &[
    "**/CLAUDE.md",
    "**/CLAUDE.local.md",
    "**/AGENTS.md",
    "**/AGENTS.local.md",
    "**/GEMINI.md",
    "**/.claude/**",
    "**/.agents/**",
    "**/.cursor/**",
    "**/.windsurf/**",
    "**/.opencode/**",
    "**/.ai-rulez/**",
    ".github/copilot-instructions.md",
];

/// Vendored, generated and build output plus minified and map files.
pub const VENDORED_BUILD_GLOBS: &[&str] = &[
    "**/node_modules/**",
    "**/vendor/**",
    "**/third_party/**",
    "**/dist/**",
    "**/build/**",
    "**/target/**",
    "**/generated/**",
    "**/__generated__/**",
    "**/*.min.*",
    "**/*.map",
];

/// Lockfiles and vector images: bulk with no prose.
pub const LOCKFILE_GLOBS: &[&str] = &[
    "*.svg",
    "pnpm-lock.yaml",
    "package-lock.json",
    "yarn.lock",
    "Cargo.lock",
    "poetry.lock",
    "*.lock",
];

/// The default `[documents] exclude` list: lockfiles, tests, agent config and vendored or build
/// output. `basemind init` writes it into the generated `[documents]` block so it is visible and
/// editable; a user entry `!pattern` re-includes a path the list drops.
pub fn default_document_exclude() -> Vec<String> {
    [
        LOCKFILE_GLOBS,
        TEST_PATH_GLOBS,
        AGENT_CONFIG_GLOBS,
        VENDORED_BUILD_GLOBS,
    ]
    .into_iter()
    .flatten()
    .map(|glob| (*glob).to_string())
    .collect()
}

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
    /// are not extracted, chunked or searchable as documents. Same syntax as `include`, and the
    /// list is ordered like a `.gitignore`: a later `!pattern` entry re-includes what an earlier
    /// entry dropped (`["**/tests/**", "!tests/docs/**"]`). The default drops low-value bulk:
    /// `*.svg`, lockfiles, test, fixture, mock, snapshot and e2e paths, agent instruction files and
    /// directories (`CLAUDE.md`, `AGENTS.md`, `.claude/`, `.cursor/`, ...), and vendored or build
    /// output (`node_modules`, `vendor`, `dist`, `build`, `*.min.*`, `*.map`). Setting the key
    /// replaces the default list; `exclude = []` indexes all of them, and a custom list must repeat
    /// any default entry it still wants (or append `!` entries after `basemind init` wrote the
    /// default list out). `include`, `embed_include` and `embed_exclude` accept the same `!` form.
    /// Per-extension rules need no extra syntax: `exclude = ["**/*.html", "!docs/**/*.html"]` drops
    /// HTML everywhere except under `docs/`.
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
        default_document_exclude()
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
    /// Run OCR on raster images. Off by default: screenshots and diagrams add embedding time and
    /// rarely answer a question. Takes effect only in a binary built with the `documents-ocr`
    /// feature. It does not gate scanned PDFs: a build with the OCR tier compiled in always
    /// OCRs them.
    #[serde(default)]
    pub enabled: bool,
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
            enabled: false,
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
}
