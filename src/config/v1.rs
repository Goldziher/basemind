use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::code::CodeSearchConfig;
use super::comms::CommsConfig;
use super::documents::{DocumentsConfig, LlmConfig};
use super::shells::ShellsConfig;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigV1 {
    #[serde(rename = "$schema")]
    pub schema: String,
    #[serde(default)]
    pub scan: ScanConfig,
    #[serde(default)]
    pub code_intel: CodeIntelConfig,
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
    /// Semantic code-search tier: chunk + embed source for the `search_code` MCP tool.
    /// Inert unless the `code-search` cargo feature is compiled in.
    #[serde(default)]
    pub code_search: CodeSearchConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub comms: CommsConfig,
    #[serde(default)]
    pub crawl: CrawlConfig,
    /// Visual / headless agent-shell presentation config. Consumed by the `shells` feature's
    /// visual launcher; the schema is stable regardless of feature gating.
    #[serde(default)]
    pub shells: ShellsConfig,
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
    /// Follow symlinks during the repository walk. Default `false` — symlinks are a common way to
    /// escape the repo (Bazel's `bazel-*` convenience symlinks point into an external output tree),
    /// and following them can balloon the scan or pull in unrelated files. Set `true` for repos that
    /// deliberately symlink real source into place; the exclude floor still prunes `bazel-*` so a
    /// symlinked Bazel tree does not leak in. `extra_roots` always follow symlinks regardless of this
    /// flag (Bazel `external/` is symlink-heavy and is opted into explicitly).
    #[serde(default = "ScanConfig::default_follow_symlinks")]
    pub follow_symlinks: bool,
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
    /// Extra directories to index in addition to the repository root. Each entry is an
    /// absolute path to a directory *outside* the repo — e.g. a Bazel external repo cache
    /// (`/private/var/tmp/_bazel_<user>/<hash>/external`) — whose files should resolve in
    /// symbol search, references, outlines, and document search.
    ///
    /// Files under an extra root are keyed by their **absolute** path (repo files stay
    /// repo-relative), so returned paths for external files are absolute. Missing or
    /// unreadable roots are skipped with a warning; a root inside the repo is ignored (the
    /// primary walk already covers it). Extra roots are (re-)indexed on a full `basemind
    /// scan` only — the live watcher does not track them. Symlinks are followed for extra
    /// roots (Bazel `external/` is symlink-heavy). Because these trees can be large, scope
    /// them narrowly and lean on `exclude` + `max_file_bytes`.
    #[serde(default = "ScanConfig::default_extra_roots")]
    pub extra_roots: Vec<std::path::PathBuf>,
}

impl ScanConfig {
    fn default_include() -> Vec<String> {
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
            "**/bazel-out/**",
            "**/bazel-bin/**",
            "**/bazel-testlogs/**",
            "**/bazel-*/**",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
    }
    fn default_respect_gitignore() -> bool {
        true
    }
    fn default_follow_symlinks() -> bool {
        false
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
    fn default_extra_roots() -> Vec<std::path::PathBuf> {
        Vec::new()
    }
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            include: Self::default_include(),
            exclude: Self::default_exclude(),
            respect_gitignore: Self::default_respect_gitignore(),
            follow_symlinks: Self::default_follow_symlinks(),
            max_file_bytes: Self::default_max_file_bytes(),
            skip_submodules: Self::default_skip_submodules(),
            eager_l2: Self::default_eager_l2(),
            extra_roots: Self::default_extra_roots(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeIntelConfig {
    /// Master switch for precise, scope- and import-aware name resolution. When `true` (default),
    /// languages with a precise engine resolve references to their true definitions — JS/TS via oxc
    /// (`code-intel-js`), Python/Java via the stack-graphs `.tsg` engine (`code-intel-stack`) — so
    /// `find_references` / `find_callers` / `goto_definition` distinguish a shadowed local from an
    /// import instead of matching by name. Set to `false` to fall back to fast tree-sitter `locals`
    /// scope binding for every language (the precise engines are skipped). Inert for languages with
    /// no precise engine regardless. Takes effect on files (re)analyzed after the change; run a full
    /// `basemind scan` to apply it to an already-indexed repo.
    #[serde(default = "CodeIntelConfig::default_precise_resolution")]
    pub precise_resolution: bool,
}

impl CodeIntelConfig {
    fn default_precise_resolution() -> bool {
        true
    }
}

impl Default for CodeIntelConfig {
    fn default() -> Self {
        Self {
            precise_resolution: Self::default_precise_resolution(),
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
    /// Allow crawling URLs that resolve to private, loopback, or link-local
    /// addresses (`127.0.0.0/8`, `10.0.0.0/8`, `169.254.0.0/16`, …). Default
    /// `false`: the engine rejects them with an SSRF-policy violation. Flip this
    /// on only to scrape an internal docs server you control. (The
    /// `CRAWLBERG_ALLOW_PRIVATE_NETWORK` env var is honoured as a process-wide
    /// override regardless of this setting.)
    #[serde(default = "CrawlConfig::default_allow_private_network")]
    pub allow_private_network: bool,
}

impl CrawlConfig {
    fn default_respect_robots_txt() -> bool {
        true
    }
    fn default_allow_private_network() -> bool {
        false
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
            allow_private_network: Self::default_allow_private_network(),
        }
    }
}

impl ConfigV1 {
    pub fn with_defaults() -> Self {
        Self {
            schema: "v1".to_string(),
            scan: ScanConfig::default(),
            code_intel: CodeIntelConfig::default(),
            watch: WatchConfig::default(),
            cache: CacheConfig::default(),
            mcp: McpConfig::default(),
            languages: Default::default(),
            documents: DocumentsConfig::default(),
            code_search: CodeSearchConfig::default(),
            memory: MemoryConfig::default(),
            comms: CommsConfig::default(),
            crawl: CrawlConfig::default(),
            shells: ShellsConfig::default(),
            llm: LlmConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_exclude_includes_bazel_generated_trees() {
        let exclude = ScanConfig::default().exclude;
        for glob in [
            "**/bazel-out/**",
            "**/bazel-bin/**",
            "**/bazel-testlogs/**",
            "**/bazel-*/**",
        ] {
            assert!(
                exclude.iter().any(|e| e == glob),
                "default exclude is missing {glob}: {exclude:?}"
            );
        }
    }
}
