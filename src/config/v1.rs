use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::code::CodeSearchConfig;
use super::comms::CommsConfig;
use super::documents::{DocumentsConfig, LlmConfig};
use super::resources::ResourcesConfig;

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
    /// Per-grammar overrides keyed by tree-sitter-language-pack grammar name (`[languages.jinja2]`):
    /// disable a grammar, map extra extensions / file names onto it, or pre-fetch it.
    #[serde(default)]
    pub languages: std::collections::BTreeMap<String, LanguageConfig>,
    #[serde(default)]
    pub documents: DocumentsConfig,
    /// Resource-governance knobs: scanner / embedder thread caps, document
    /// concurrency, embed batch size, footprint ceiling, and the document model
    /// profile. Bounds basemind's footprint on constrained machines.
    #[serde(default)]
    pub resources: ResourcesConfig,
    /// Semantic code-search tier: chunk + embed source for the `code_search` tool.
    /// Inert unless the `code-search` cargo feature is compiled in.
    #[serde(default)]
    pub code_search: CodeSearchConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub comms: CommsConfig,
    /// Deprecated and ignored: web crawling was removed. Accepted so files that still carry a
    /// `[crawl]` table (`basemind init` used to write one) keep loading.
    #[serde(default, skip_serializing_if = "DeprecatedSection::is_empty")]
    pub crawl: DeprecatedSection,
    /// Deprecated and ignored: agent shells were removed. Accepted so files that still carry a
    /// `[shells]` table keep loading.
    #[serde(default, skip_serializing_if = "DeprecatedSection::is_empty")]
    pub shells: DeprecatedSection,
    /// Deprecated and ignored: LLM summarization and LLM entity recognition were removed and
    /// nothing else read this table. Accepted so existing files keep loading; a table with content
    /// logs a warning.
    #[serde(default)]
    pub llm: LlmConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScanConfig {
    /// Allow-list of globs (repo-relative, forward-slash, case-sensitive; `*` crosses `/`). A file
    /// is scanned only when it matches at least one entry, so an empty list is rejected at load
    /// (it would silently index nothing). A bare name with no glob metacharacters (`src`,
    /// `docs/api`) matches that path and everything beneath it, like a `.gitignore` entry.
    #[serde(default = "ScanConfig::default_include")]
    #[schemars(length(min = 1), inner(length(min = 1)))]
    pub include: Vec<String>,
    /// Deny-list of globs, same syntax as `include`; exclusion beats inclusion. Applied on top of
    /// the always-on exclude floor (`build`, `out`, `vendor`, `coverage`, `venv`, `dist`, `target`,
    /// ... see `floor_allow`). A bare name (`generated`) excludes every path segment called that,
    /// and everything beneath it. For `extra_roots` files the globs match the path relative to the
    /// extra root, not the absolute path.
    #[serde(default = "ScanConfig::default_exclude")]
    #[schemars(inner(length(min = 1)))]
    pub exclude: Vec<String>,
    /// Entries to remove from the always-on exclude floor, named by directory (`build`, `vendor`)
    /// or by the floor pattern itself (`**/build/**`). Use it to index a tree the floor otherwise
    /// drops. The default `exclude` list is separate: a directory also listed there (`dist`,
    /// `target`) must be removed from `exclude` too. The floor also drops credential files (`.env`,
    /// `.env.*`, `.aws`, `.ssh`, `.gnupg`, `.npmrc`, `id_rsa`, `*.pem`, `*.key`, ...); list the
    /// entry (`.env.*`, `*.pem`) to index one deliberately. A credential entry here is honoured only
    /// when the operator sets `BASEMIND_ALLOW_REPO_CREDENTIALS`, so a repository's own file cannot
    /// un-exclude the operator's secrets. `.git` and `.basemind` can never be allowed,
    /// and an entry naming nothing in the floor is ignored with a warning.
    #[serde(default)]
    #[schemars(inner(length(min = 1)))]
    pub floor_allow: Vec<String>,
    #[serde(default = "ScanConfig::default_respect_gitignore")]
    pub respect_gitignore: bool,
    /// Follow symlinks during the walk. Ignored (forced `false`) when set by the repository's own
    /// `basemind.toml` unless the operator sets `BASEMIND_ALLOW_FOLLOW_SYMLINKS=1`. Default `false` — symlinks are a common way to escape the
    /// repo (Bazel's `bazel-*` convenience symlinks point into an external output tree), and
    /// following them can balloon the scan or pull in unrelated files. Set `true` for repos that
    /// deliberately symlink real source into place; the exclude floor still prunes `bazel-*` so a
    /// symlinked Bazel tree does not leak in. This governs `extra_roots` too: those walks used to
    /// follow links unconditionally, which let a link inside a directory named by the *repository's
    /// own* config reach anywhere on the filesystem.
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
    /// L2 populates the `calls_by_callee` Fjall partition that drives `code_refs`. Flipping to
    /// `false` halves the scan budget on large repos at the cost of empty reference-search results
    /// until a foreground L2 pass is triggered (or the existing on-demand
    /// `query::file_outline_l2` lazy path runs).
    #[serde(default = "ScanConfig::default_eager_l2")]
    pub eager_l2: bool,
    /// Extra directories to index in addition to the repository root. Each entry is an
    /// absolute path to a directory *outside* the repo — e.g. a Bazel external repo cache
    /// (`/private/var/tmp/_bazel_<user>/<hash>/external`) — whose files should resolve in
    /// symbol search, references, outlines, and document search.
    ///
    /// **Requires the `BASEMIND_ALLOW_EXTRA_ROOTS` environment grant.** This key is read from
    /// the scanned repository's own `basemind.toml`, so without an operator-side opt-in a cloned
    /// repository could point basemind at `~/.ssh` and read the results back out through the
    /// search tools. Unset, every entry here is ignored with a warning. `1` grants every
    /// workspace the process scans (including every later workspace of a long-lived daemon); a
    /// path list (`:`-separated) of workspace roots grants only those workspaces.
    ///
    /// Files under an extra root are keyed by their **absolute** path (repo files stay
    /// repo-relative), so returned paths for external files are absolute. Missing or
    /// unreadable roots are skipped with a warning; so is a root inside the repo (the primary
    /// walk already covers it) and a filesystem or volume root (never indexable, grant or no
    /// grant). Extra roots are (re-)indexed on a full `basemind scan` only — the live watcher
    /// does not track them — they count toward `max_candidates` like any other candidate, and
    /// they follow symlinks only when `follow_symlinks` is on. Because these trees can be large,
    /// scope them narrowly and lean on `exclude` + `max_file_bytes`.
    #[serde(default = "ScanConfig::default_extra_roots")]
    pub extra_roots: Vec<std::path::PathBuf>,
    /// Ceiling on how many candidate files one scan may **keep**: files that passed the
    /// include/exclude gate and would be indexed, across the repository walk and every
    /// `extra_roots` walk. Exceeding it aborts before any extraction, any index write, and before
    /// the candidate list can grow to hundreds of megabytes, and the error names the heaviest
    /// contributing directories — so a vendored or generated tree that slipped past `.gitignore`
    /// and `exclude` is a one-line fix instead of an OOM kill (issue #62).
    ///
    /// It also bounds how far the walk may travel to find those candidates: a walk that visits
    /// more than a generous multiple of this many filesystem entries aborts too, with a distinct
    /// error, because a root broad enough to enumerate tens of millions of entries burns the time
    /// and syscalls this ceiling exists to prevent even when it keeps almost nothing.
    ///
    /// What it does **not** bound: bytes read, index size, or the `Staged` / `Rev` scan sources,
    /// which enumerate from git rather than from a walk. `0` disables both bounds.
    #[serde(default = "ScanConfig::default_max_candidates")]
    pub max_candidates: usize,
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
    fn default_max_candidates() -> usize {
        500_000
    }
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            include: Self::default_include(),
            exclude: Self::default_exclude(),
            floor_allow: Vec::new(),
            respect_gitignore: Self::default_respect_gitignore(),
            follow_symlinks: Self::default_follow_symlinks(),
            max_file_bytes: Self::default_max_file_bytes(),
            skip_submodules: Self::default_skip_submodules(),
            eager_l2: Self::default_eager_l2(),
            extra_roots: Self::default_extra_roots(),
            max_candidates: Self::default_max_candidates(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeIntelConfig {
    /// Master switch for precise, scope- and import-aware name resolution. When `true` (default),
    /// languages with a precise engine resolve references to their true definitions — JS/TS via oxc
    /// (`code-intel-js`), Python/Java via the stack-graphs `.tsg` engine (`code-intel-stack`) — so
    /// `code_refs` (pinned callers and the `definition` relation) distinguishes a shadowed local
    /// from an import instead of matching by name. Set to `false` to fall back to fast tree-sitter `locals`
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
    /// Reserved: parsed but currently has no effect (a load-time warning is logged when set).
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
    /// Reserved: parsed but currently has no effect (a load-time warning is logged when set). The
    /// outline cache is bounded by `resources.max_map_cache_mb`.
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
    /// Reserved: `stdio` is the only variant and nothing reads it.
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
    /// `false` disables the language: its files are no longer parsed as code and are handled as if
    /// no grammar recognised them (so they fall through to the document tier, subject to
    /// `[documents]`). Use it for tree-sitter-language-pack misdetections such as `.txt` (vimdoc)
    /// or `.conf` (nginx). Already-indexed files of a disabled language are dropped on the next scan.
    #[serde(default = "LanguageConfig::default_enabled")]
    pub enabled: bool,
    /// Extra file suffixes mapped to this grammar, e.g. `[".mako", ".tpl"]` (leading dot optional,
    /// case-insensitive, compound suffixes like `.html.erb` allowed). Overrides take precedence over
    /// built-in detection. The table key must be a tree-sitter-language-pack grammar name.
    #[serde(default)]
    #[schemars(inner(length(min = 1)))]
    pub extensions: Vec<String>,
    /// Exact file names mapped to this grammar, e.g. `["BUILD.in"]`. Takes precedence over
    /// `extensions` and built-in detection.
    #[serde(default)]
    #[schemars(inner(length(min = 1)))]
    pub filenames: Vec<String>,
    /// Also fetch this grammar in `basemind lang install` (by default only the languages basemind
    /// ships queries for are pre-fetched; others download on first use).
    #[serde(default)]
    pub preload: bool,
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
            extensions: Vec::new(),
            filenames: Vec::new(),
            preload: false,
        }
    }
}

/// Memory tier selector. `group` (the default) is the shared, cross-agent tier — today's
/// behavior, with an empty owner segment. `individual` scopes the entry to the calling
/// agent (owner = its `AgentId`), so two agents can keep private same-key entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Shared, cross-agent memory (owner segment is empty). The default.
    #[default]
    Group,
    /// Per-agent memory (owner segment is the caller's `AgentId`).
    Individual,
}

impl JsonSchema for Visibility {
    /// Inlined, not `$ref`'d into `$defs` — see [`crate::path::RelPath`]'s impl and GH #50.
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Visibility".into()
    }

    /// A flat string enum, written by hand rather than derived.
    ///
    /// The derive turns per-variant doc comments into `oneOf: [{const, description}, …]`, and
    /// `oneOf` is outside the Anthropic `input_schema` subset — which silently drops the server's
    /// entire tool registry (GH #50). The variant docs live in the description instead, so the
    /// meaning survives without the rejected construct.
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "enum": ["group", "individual"],
            "description": "Memory tier: `group` (default) is shared across agents; `individual` \
                            scopes the entry to the calling agent."
        })
    }
}

impl Visibility {
    /// Stable, append-only on-disk ordinal for this tier — matches the `vis_byte`
    /// encoded by [`crate::index::keys::memory_by_key`].
    pub fn vis_byte(self) -> u8 {
        match self {
            Visibility::Group => crate::index::keys::MEMORY_VIS_GROUP,
            Visibility::Individual => crate::index::keys::MEMORY_VIS_INDIVIDUAL,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    /// Reserved: parsed but currently has no effect (a load-time warning is logged when set to a
    /// non-default value); the `memory` cargo feature alone decides whether memory is available.
    #[serde(default = "MemoryConfig::default_enabled")]
    pub enabled: bool,
    /// Reserved: parsed but currently has no effect (a load-time warning is logged when set to a
    /// non-default value). Would choose how the scope key of an opened repository is derived.
    #[serde(default)]
    pub scope_strategy: MemoryScopeStrategy,
    /// Reserved: parsed but currently has no effect (a load-time warning is logged when set to a
    /// non-default value). Would set the default memory tier when a `memory_*` call omits
    /// `visibility`.
    #[serde(default)]
    pub default_visibility: Visibility,
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
            default_visibility: Visibility::default(),
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

/// A table that configured a removed feature. Any content is accepted and nothing is read from it,
/// so a `basemind.toml` written for an older version keeps loading under the strict schema.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(extend("deprecated" = true))]
pub struct DeprecatedSection(serde_json::Map<String, serde_json::Value>);

impl DeprecatedSection {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
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
            resources: ResourcesConfig::default(),
            code_search: CodeSearchConfig::default(),
            memory: MemoryConfig::default(),
            comms: CommsConfig::default(),
            crawl: DeprecatedSection::default(),
            shells: DeprecatedSection::default(),
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
