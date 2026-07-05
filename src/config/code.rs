//! `[code_search]` config table — knobs for the semantic code-search tier.
//!
//! Split from `v1.rs` to keep both files under the 1000-line cap. Every field has
//! `#[serde(default)]` so adding this table never breaks an older TOML file, and the whole
//! table is inert unless the `code-search` cargo feature is compiled in.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `[code_search]` table. Chunk + embed source code for the `search_code` MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeSearchConfig {
    /// Master switch. Only meaningful when the `code-search` cargo feature is compiled in.
    /// Default `true` — a `code-search` build chunks + embeds source on scan unless disabled.
    #[serde(default = "CodeSearchConfig::default_enabled")]
    pub enabled: bool,
    /// Maximum chunk size in characters. Chunks longer than this are split into overlapping
    /// windows. Mirrors the document tier's `max_characters`.
    #[serde(default = "CodeSearchConfig::default_max_characters")]
    #[schemars(range(min = 64))]
    pub max_characters: usize,
    /// Overlap between split windows, in characters. Mirrors the document tier's `overlap`.
    #[serde(default = "CodeSearchConfig::default_overlap")]
    #[schemars(range(min = 0))]
    pub overlap: usize,
    /// Generate embeddings (`true`) or chunk-only without vector storage (`false`). With
    /// embeddings off the `.chunk.msgpack` cache is still written, but no LanceDB rows land and
    /// `search_code` returns nothing.
    #[serde(default = "CodeSearchConfig::default_embed")]
    pub embed: bool,
}

impl CodeSearchConfig {
    /// Cross-field validation the per-field JSON-schema bounds can't express: `overlap` must be
    /// strictly less than `max_characters`. When `overlap >= max_characters` the chunker's
    /// `split_oversized` computes `step = max_characters.saturating_sub(overlap).max(1) = 1`,
    /// emitting one degenerate window per character of an oversized chunk instead of a handful.
    /// Returns a human-readable error naming both offending values on violation.
    pub fn validate(&self) -> Result<(), String> {
        if self.overlap >= self.max_characters {
            return Err(format!(
                "[code_search] overlap ({}) must be less than max_characters ({}); an overlap \
                 >= max_characters collapses the chunker step to 1 character",
                self.overlap, self.max_characters
            ));
        }
        Ok(())
    }

    fn default_enabled() -> bool {
        true
    }
    fn default_max_characters() -> usize {
        1500
    }
    fn default_overlap() -> usize {
        200
    }
    fn default_embed() -> bool {
        true
    }
}

impl Default for CodeSearchConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            max_characters: Self::default_max_characters(),
            overlap: Self::default_overlap(),
            embed: Self::default_embed(),
        }
    }
}
