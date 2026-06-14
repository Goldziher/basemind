//! Provenance tagging for a resolved config value. Captures *which layer* of
//! the precedence stack produced each field so introspection (`--help`-style
//! diagnostics, telemetry) can explain why a value is what it is.
//!
//! Precedence — highest wins: `Mcp` > `Cli` > `Env` > `File` > `Default`.

use serde::{Deserialize, Serialize};

/// Origin of a resolved config field. Internal-only — never appears in the
/// JSON Schema (`JsonSchema` deliberately not derived).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    /// Hard-coded `Default::default()` value.
    Default,
    /// Loaded from `.basemind/basemind.toml`.
    File,
    /// Override pulled from an environment variable (`BASEMIND_*`).
    Env,
    /// Override passed via a CLI flag (`--documents-*`).
    Cli,
    /// Override passed via per-call MCP params (iter 3 — handled inside the tool body).
    Mcp,
}

/// Dotted-path → source map. Keys look like `"documents.reranker.preset"`.
/// `BTreeMap` keeps iteration order deterministic for snapshot tests.
pub type ProvenanceMap = std::collections::BTreeMap<&'static str, ConfigSource>;
