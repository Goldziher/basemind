//! Resource-governance config (`[resources]`). Split from `v1.rs` into its own
//! module so the memory / concurrency knobs stay together as they grow — the
//! same split shape the `[documents]` tier already uses.
//!
//! This tier is the single place an operator bounds basemind's footprint on a
//! constrained machine: how many threads the code-map scanner and the ONNX
//! embedder may use, how large a batch the embedder builds, and the hard
//! `max_footprint_mb` ceiling the best-effort backpressure gate
//! ([`crate::backpressure::FootprintGate`]) throttles the scan against.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Top-level `[resources]` table. Every field has `#[serde(default)]` (directly
/// or via a default fn) so adding a knob never breaks an older TOML file, and an
/// omitted `[resources]` section deserialises to [`ResourcesConfig::default`].
///
/// `0` is the "auto" sentinel for the thread / concurrency caps: it means "let
/// basemind pick a bounded fraction of the machine" rather than "use zero
/// threads". This keeps the default config safe on a laptop while letting an
/// operator pin an explicit budget on a shared box.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourcesConfig {
    /// Cap on the code-map scanner's rayon pool. `0` (auto) keeps rayon's
    /// default (one worker per logical CPU); a non-zero value pins the pool so
    /// the scan can't saturate every core on a shared machine. First scan wins
    /// for the process — the pool is built once and its size is then fixed.
    #[serde(default)]
    pub scan_threads: usize,
    /// Cap on the ONNX embedding pool. `0` (auto) resolves to
    /// `max(2, logical_cpus / 4)` via `crate::embeddings::resolve_embed_threads`
    /// — a bounded fraction of cores so the embedder never pins the machine and
    /// ORT arenas are not replicated across every core. This supersedes the
    /// deprecated `[documents].embed_max_threads`; see
    /// [`ResourcesConfig::effective_embed_threads`] for the precedence.
    #[serde(default)]
    pub embed_threads: usize,
    /// Upper bound on documents extracted concurrently. `0` (auto) leaves the
    /// dispatch unbounded (today's behaviour). Parsed now; a concurrency
    /// semaphore around document dispatch is a later iteration — the field
    /// exists so the schema is stable ahead of the consumer. Memory is bounded
    /// today by `max_footprint_mb` rather than by an in-flight document count.
    #[serde(default)]
    pub max_concurrent_documents: usize,
    /// Number of chunks the embedder submits to ONNX per batch. Larger batches
    /// amortise per-call overhead at the cost of a higher transient memory
    /// spike; 32 is a safe default across the preset models. Threaded into both
    /// `SharedEmbedder` (code-search + query paths) and the document extractor's
    /// `EmbeddingConfig`.
    #[serde(default = "ResourcesConfig::default_embed_batch_size")]
    pub embed_batch_size: usize,
    /// Hard ceiling on process physical footprint in mebibytes. `0` (disabled)
    /// is the default. When set, the best-effort backpressure gate
    /// ([`crate::backpressure::FootprintGate`]) samples
    /// [`crate::sysres::phys_footprint`] at the document-extraction and
    /// chunk-embedding admit points and parks the worker while the process is
    /// over this ceiling, shaving the scan's peak. It is best-effort: an
    /// unreadable sample or a sustained overshoot admits anyway to guarantee
    /// forward progress, so the ceiling shapes the peak rather than enforcing a
    /// hard invariant. macOS today (`phys_footprint` is a Darwin metric); a
    /// no-op elsewhere.
    #[serde(default)]
    pub max_footprint_mb: usize,
    /// Which model families run during document extraction. `Full` (default)
    /// runs every configured post-processor; the narrower profiles strip
    /// enrichment / embeddings to shrink the scan-time footprint on code-centric
    /// workspaces. See [`DocumentModelProfile`].
    #[serde(default)]
    pub document_models: DocumentModelProfile,
}

impl ResourcesConfig {
    /// Default embedding batch size. 32 balances ONNX per-call amortisation
    /// against the transient memory spike of a larger batch.
    fn default_embed_batch_size() -> usize {
        32
    }

    /// Resolve the effective ONNX embed-thread cap, honouring the deprecated
    /// `[documents].embed_max_threads` alias for back-compat.
    ///
    /// Precedence: `resources.embed_threads` wins whenever it is set (non-zero);
    /// otherwise the deprecated alias is consulted; `0` from both means "auto"
    /// (resolved downstream by `crate::embeddings::resolve_embed_threads`). This
    /// lets existing configs that still set `[documents].embed_max_threads` keep
    /// working while new configs use the `[resources]` home for the knob.
    pub fn effective_embed_threads(&self, deprecated_alias: usize) -> usize {
        if self.embed_threads != 0 {
            self.embed_threads
        } else {
            deprecated_alias
        }
    }
}

impl Default for ResourcesConfig {
    fn default() -> Self {
        Self {
            scan_threads: 0,
            embed_threads: 0,
            max_concurrent_documents: 0,
            embed_batch_size: Self::default_embed_batch_size(),
            max_footprint_mb: 0,
            document_models: DocumentModelProfile::default(),
        }
    }
}

/// Selects which model families run during document extraction, trading recall
/// for a smaller scan-time footprint on workspaces that are mostly source code.
///
/// The enrichment post-processors (keyword extraction, NER, summarisation) and
/// OCR each pull in their own ONNX / LLM weights; a code workspace rarely wants
/// any of them. Narrowing the profile lets an operator keep the code map and
/// (optionally) embeddings while paying for nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum DocumentModelProfile {
    /// Run every configured capability (embeddings + keywords + NER +
    /// summarisation + OCR). The default — no behaviour change from before this
    /// knob existed.
    #[default]
    Full,
    /// Embeddings only: chunks are still embedded per the `[documents]` config,
    /// but keyword extraction, NER, and summarisation are forced off and OCR is
    /// disabled. The lever for a code-centric workspace that still wants
    /// semantic document search.
    CodeOnly,
    /// Metadata only: no embeddings and no enrichment post-processors, OCR
    /// disabled. Documents are extracted for their text + metadata (keyword
    /// search) but never routed to any model. Serialised as `"none"` (the Rust
    /// identifier carries a trailing underscore only because `None` collides
    /// with `Option::None`).
    #[serde(rename = "none")]
    None_,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_resources_config_has_expected_field_values() {
        let cfg = ResourcesConfig::default();
        assert_eq!(cfg.scan_threads, 0);
        assert_eq!(cfg.embed_threads, 0);
        assert_eq!(cfg.max_concurrent_documents, 0);
        assert_eq!(cfg.embed_batch_size, 32);
        assert_eq!(cfg.max_footprint_mb, 0);
        assert_eq!(cfg.document_models, DocumentModelProfile::Full);
    }

    #[test]
    fn document_model_profile_defaults_to_full() {
        assert_eq!(DocumentModelProfile::default(), DocumentModelProfile::Full);
    }

    #[test]
    fn resources_toml_roundtrips_embed_batch_size_override() {
        let cfg: ResourcesConfig = toml::from_str("embed_batch_size = 8\n").expect("parse [resources] body");
        assert_eq!(cfg.embed_batch_size, 8);
        // Unset fields still take their defaults.
        assert_eq!(cfg.scan_threads, 0);
        assert_eq!(cfg.document_models, DocumentModelProfile::Full);
    }

    #[test]
    fn resources_empty_toml_falls_back_to_all_defaults() {
        let cfg: ResourcesConfig = toml::from_str("").expect("empty [resources] body");
        assert_eq!(cfg.embed_batch_size, 32);
        assert_eq!(cfg.scan_threads, 0);
        assert_eq!(cfg.embed_threads, 0);
        assert_eq!(cfg.max_concurrent_documents, 0);
        assert_eq!(cfg.max_footprint_mb, 0);
        assert_eq!(cfg.document_models, DocumentModelProfile::Full);
    }

    #[test]
    fn document_model_profile_none_serializes_as_none_string() {
        let profile = DocumentModelProfile::None_;
        let json = serde_json::to_string(&profile).expect("serialize");
        assert_eq!(json, "\"none\"");
        let back: DocumentModelProfile = serde_json::from_str("\"none\"").expect("deserialize");
        assert_eq!(back, DocumentModelProfile::None_);
    }

    #[test]
    fn document_model_profile_code_only_uses_snake_case() {
        let json = serde_json::to_string(&DocumentModelProfile::CodeOnly).expect("serialize");
        assert_eq!(json, "\"code_only\"");
    }

    #[test]
    fn effective_embed_threads_prefers_resources_then_deprecated_alias() {
        // resources wins when set
        let cfg = ResourcesConfig {
            embed_threads: 4,
            ..ResourcesConfig::default()
        };
        assert_eq!(cfg.effective_embed_threads(8), 4);
        // falls back to the deprecated alias when resources is auto (0)
        let cfg = ResourcesConfig::default();
        assert_eq!(cfg.effective_embed_threads(8), 8);
        // both auto → 0 (resolved downstream)
        assert_eq!(cfg.effective_embed_threads(0), 0);
    }
}
