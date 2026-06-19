use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::comms::CommsConfig;
use super::documents::{DocumentsConfig, LlmConfig};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigV1 {
    #[serde(rename = "$schema")]
    pub schema: String,
    #[serde(default)]
    pub scan: ScanConfig,
    #[serde(default)]
    pub watch: WatchConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub languages: std::collections::BTreeMap<String, LanguageConfig>,
    #[serde(default)]
    pub documents: DocumentsConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub comms: CommsConfig,
    #[serde(default)]
    pub crawl: CrawlConfig,
    /// Shared LLM configuration. Consumed by reranker-llm, ner-llm, summarization-llm,
    /// VLM OCR, and any future LLM-backed capability. Off by default — leaving
    /// `api_key` `Unset` short-circuits any LLM-backed feature.
    #[serde(default)]
    pub llm: LlmConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScanConfig {
    #[serde(default = "ScanConfig::default_include")]
    #[schemars(inner(length(min = 1)))]
    pub include: Vec<String>,
    #[serde(default = "ScanConfig::default_exclude")]
    #[schemars(inner(length(min = 1)))]
    pub exclude: Vec<String>,
    #[serde(default = "ScanConfig::default_respect_gitignore")]
    pub respect_gitignore: bool,
    /// Skip files larger than this. Prevents minified-bundle stalls.
    #[serde(default = "ScanConfig::default_max_file_bytes")]
    #[schemars(range(min = 1024))]
    pub max_file_bytes: u64,
    /// When true (default), the scanner skips paths under any submodule root listed in
    /// `.gitmodules`. Set to false to recurse into submodule working trees — useful only
    /// if you want one combined index across the parent + its embedded repos.
    #[serde(default = "ScanConfig::default_skip_submodules")]
    pub skip_submodules: bool,
    /// When true (default), the scanner runs L2 extraction (calls + docs) inline with L1.
    /// L2 populates the `calls_by_callee` Fjall partition that drives `find_references` and
    /// `find_callers`. Flipping to `false` halves the scan budget on large repos at the cost
    /// of empty reference-search results until a foreground L2 pass is triggered (or the
    /// existing on-demand `query::file_outline_l2` lazy path runs).
    #[serde(default = "ScanConfig::default_eager_l2")]
    pub eager_l2: bool,
}

impl ScanConfig {
    fn default_include() -> Vec<String> {
        // The language gate is `lang::detect()` (the tree-sitter-language-pack registry),
        // not a hand-curated glob list. Default to "any file" and let the scanner's
        // per-file detect + binary check + size cap filter the long tail. Users who want
        // to narrow can still override `[scan.include]` in their `.basemind/basemind.toml`.
        vec!["**/*".to_string()]
    }
    fn default_exclude() -> Vec<String> {
        [
            "**/target/**",
            "**/node_modules/**",
            "**/dist/**",
            "**/.venv/**",
            "**/.basemind/**",
            "**/.git/**",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
    }
    fn default_respect_gitignore() -> bool {
        true
    }
    fn default_max_file_bytes() -> u64 {
        2_097_152
    }
    fn default_skip_submodules() -> bool {
        true
    }
    fn default_eager_l2() -> bool {
        true
    }
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            include: Self::default_include(),
            exclude: Self::default_exclude(),
            respect_gitignore: Self::default_respect_gitignore(),
            max_file_bytes: Self::default_max_file_bytes(),
            skip_submodules: Self::default_skip_submodules(),
            eager_l2: Self::default_eager_l2(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchConfig {
    /// Coalesce file events within this window (milliseconds).
    #[serde(default = "WatchConfig::default_debounce_ms")]
    #[schemars(range(min = 0, max = 60000))]
    pub debounce_ms: u64,
    #[serde(default)]
    pub live_l2: bool,
}

impl WatchConfig {
    fn default_debounce_ms() -> u64 {
        250
    }
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            debounce_ms: Self::default_debounce_ms(),
            live_l2: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// Maximum number of extracted FileMaps to keep hot in memory.
    #[serde(default = "CacheConfig::default_file_map_lru")]
    #[schemars(range(min = 0))]
    pub file_map_lru: usize,
}

impl CacheConfig {
    fn default_file_map_lru() -> usize {
        256
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            file_map_lru: Self::default_file_map_lru(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default = "McpConfig::default_transport")]
    pub transport: McpTransport,
}

impl McpConfig {
    fn default_transport() -> McpTransport {
        McpTransport::Stdio
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            transport: Self::default_transport(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LanguageConfig {
    #[serde(default = "LanguageConfig::default_enabled")]
    pub enabled: bool,
}

impl LanguageConfig {
    fn default_enabled() -> bool {
        true
    }
}

impl Default for LanguageConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    /// Master switch. Only meaningful when the `memory` cargo feature is compiled in.
    #[serde(default = "MemoryConfig::default_enabled")]
    pub enabled: bool,
    /// How to derive the scope key for an opened repository.
    #[serde(default)]
    pub scope_strategy: MemoryScopeStrategy,
    /// Default memory tier when a `memory_*` call omits `visibility`. `group` (shared) keeps
    /// today's behavior; set to `individual` so a user's writes default to their private tier.
    #[serde(default)]
    pub default_visibility: crate::mcp::params::Visibility,
}

impl MemoryConfig {
    fn default_enabled() -> bool {
        true
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            scope_strategy: MemoryScopeStrategy::default(),
            default_visibility: crate::mcp::params::Visibility::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScopeStrategy {
    /// Use normalized `origin` remote URL when available, else fall back to the
    /// workdir realpath. This is the recommended default — clones of the same
    /// repo share memory across machines.
    #[default]
    GitRemoteWithFallback,
    /// Always use the workdir realpath. Separates clones of the same repo.
    WorkdirOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CrawlConfig {
    // Field-level schema validation kept in sync with the hand-rolled bounds in
    // the previous schema: bytes minima, depth minima, etc.
    /// Honour `robots.txt` when fetching pages. Default `true`. Override only
    /// for hosts you control — flipping this off violates `robots.txt`
    /// directives for every domain the crawler touches.
    #[serde(default = "CrawlConfig::default_respect_robots_txt")]
    pub respect_robots_txt: bool,
    /// Hard cap on pages visited in a single `web_crawl` call.
    #[serde(default = "CrawlConfig::default_max_pages")]
    #[schemars(range(min = 1))]
    pub max_pages: u32,
    /// Maximum link-following depth from the seed URL during `web_crawl`.
    #[serde(default = "CrawlConfig::default_max_depth")]
    #[schemars(range(min = 0))]
    pub max_depth: u32,
    /// Truncate response bodies above this many bytes before parsing.
    #[serde(default = "CrawlConfig::default_max_body_size")]
    #[schemars(range(min = 1024))]
    pub max_body_size: u64,
    /// User-Agent header sent with every request. Override to identify your
    /// crawler to operators; the default includes the basemind release
    /// version + the upstream repo URL so site operators can trace traffic.
    #[serde(default = "CrawlConfig::default_user_agent")]
    #[schemars(length(min = 1))]
    pub user_agent: String,
}

impl CrawlConfig {
    fn default_respect_robots_txt() -> bool {
        true
    }
    fn default_max_pages() -> u32 {
        32
    }
    fn default_max_depth() -> u32 {
        2
    }
    fn default_max_body_size() -> u64 {
        4 * 1024 * 1024
    }
    fn default_user_agent() -> String {
        format!(
            "basemind/{} (+https://github.com/Goldziher/basemind)",
            env!("CARGO_PKG_VERSION")
        )
    }
}

impl Default for CrawlConfig {
    fn default() -> Self {
        Self {
            respect_robots_txt: Self::default_respect_robots_txt(),
            max_pages: Self::default_max_pages(),
            max_depth: Self::default_max_depth(),
            max_body_size: Self::default_max_body_size(),
            user_agent: Self::default_user_agent(),
        }
    }
}

impl ConfigV1 {
    pub fn with_defaults() -> Self {
        Self {
            schema: "v1".to_string(),
            scan: ScanConfig::default(),
            watch: WatchConfig::default(),
            cache: CacheConfig::default(),
            mcp: McpConfig::default(),
            languages: Default::default(),
            documents: DocumentsConfig::default(),
            memory: MemoryConfig::default(),
            comms: CommsConfig::default(),
            crawl: CrawlConfig::default(),
            llm: LlmConfig::default(),
        }
    }
}
