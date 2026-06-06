use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanConfig {
    #[serde(default = "ScanConfig::default_include")]
    pub include: Vec<String>,
    #[serde(default = "ScanConfig::default_exclude")]
    pub exclude: Vec<String>,
    #[serde(default = "ScanConfig::default_respect_gitignore")]
    pub respect_gitignore: bool,
    #[serde(default = "ScanConfig::default_max_file_bytes")]
    pub max_file_bytes: u64,
    #[serde(default = "ScanConfig::default_max_file_lines")]
    pub max_file_lines: u64,
}

impl ScanConfig {
    fn default_include() -> Vec<String> {
        [
            "**/*.rs", "**/*.py", "**/*.ts", "**/*.tsx", "**/*.js", "**/*.go",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
    }
    fn default_exclude() -> Vec<String> {
        [
            "**/target/**",
            "**/node_modules/**",
            "**/dist/**",
            "**/.venv/**",
            "**/.gitmind/**",
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
    fn default_max_file_lines() -> u64 {
        50_000
    }
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            include: Self::default_include(),
            exclude: Self::default_exclude(),
            respect_gitignore: Self::default_respect_gitignore(),
            max_file_bytes: Self::default_max_file_bytes(),
            max_file_lines: Self::default_max_file_lines(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchConfig {
    #[serde(default = "WatchConfig::default_debounce_ms")]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    #[serde(default = "CacheConfig::default_file_map_lru")]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl ConfigV1 {
    pub fn with_defaults() -> Self {
        Self {
            schema: "v1".to_string(),
            scan: ScanConfig::default(),
            watch: WatchConfig::default(),
            cache: CacheConfig::default(),
            mcp: McpConfig::default(),
            languages: Default::default(),
        }
    }
}
