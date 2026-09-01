//! Resource-governance config (`[resources]`). Split from `v1.rs` into its own
//! module so the memory / concurrency knobs stay together as they grow — the
//! same split shape the `[documents]` tier already uses.
//!
//! This tier is the single place an operator bounds basemind's footprint on a
//! constrained machine: how many threads the code-map scanner and the ONNX
//! embedder may use, how large a batch the embedder builds, and the
//! `max_footprint_mb` ceiling the best-effort backpressure gate
//! ([`crate::backpressure::FootprintGate`]) throttles the scan against.
//!
//! That ceiling defaults to **auto** rather than off. A long-lived daemon
//! shipping with its only memory bound disabled is how a scan reached 43.8 GiB
//! and was OOM-killed (issue #62), and the machine it happened on was under a
//! cgroup `MemoryMax` the whole time. [`MaxFootprint`] therefore derives a
//! budget from what [`crate::sysres`] can see of the environment, and turning
//! the ceiling off is a thing an operator has to write down (`"off"`).

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
    /// Ceiling on process memory footprint, in mebibytes. Accepts a positive
    /// integer (an explicit ceiling), `0` or `"auto"` (derive one from the
    /// environment — the default), or `"off"` (no ceiling at all).
    ///
    /// The best-effort backpressure gate
    /// ([`crate::backpressure::FootprintGate`]) samples
    /// [`crate::sysres::phys_footprint`] at each admit point and parks the
    /// worker while the process is over the ceiling, shaving the scan's peak.
    /// It stays best-effort: an unreadable sample or a sustained overshoot
    /// admits anyway to guarantee forward progress, so the ceiling shapes the
    /// peak rather than enforcing a hard invariant.
    ///
    /// **`0` used to mean "disabled" and now means "auto".** A config carrying
    /// the old default therefore gains a ceiling on upgrade; `"off"` restores
    /// the previous behaviour. See [`MaxFootprint::auto_ceiling_bytes`] for the
    /// arithmetic.
    #[serde(default)]
    pub max_footprint_mb: MaxFootprint,
    /// Byte budget, in mebibytes, for the MCP read stack's decoded-outline cache
    /// ([`crate::mcp::l1_cache::L1Cache`]). `0` means unbounded — the
    /// pre-0.26 behaviour, where every indexed file's decoded L1 stayed
    /// resident for the process lifetime, once per hot workspace.
    ///
    /// Charged in BYTES rather than in entries because a decoded outline
    /// spans three orders of magnitude (a ~1.4 KB median against a measured
    /// 697 KB maximum), so an entry cap would bound the count and not the
    /// footprint. A miss costs one content-addressed blob read and never
    /// changes an answer. Read-only sessions charge their projected call /
    /// implementation indexes — the substitute for a Fjall index they cannot
    /// open — against the same budget, since it is the one knob bounding the
    /// read stack.
    #[serde(default = "ResourcesConfig::default_max_map_cache_mb")]
    pub max_map_cache_mb: usize,
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

    /// Default read-stack outline-cache budget. 256 MiB holds roughly 180k
    /// files at the measured median, so every repo that fits keeps today's
    /// pure-RAM read stack while a monorepo that does not is bounded.
    fn default_max_map_cache_mb() -> usize {
        256
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
            max_footprint_mb: MaxFootprint::default(),
            max_map_cache_mb: Self::default_max_map_cache_mb(),
            document_models: DocumentModelProfile::default(),
        }
    }
}

/// Bytes in one mebibyte — the unit `max_footprint_mb` is expressed in.
const BYTES_PER_MB: u64 = 1024 * 1024;

/// Fraction of an *enforced* limit (a cgroup ceiling: crossing it is an OOM
/// kill) that auto-mode budgets to basemind. A quarter of headroom covers the
/// allocator's lag behind `free`, the page cache the working-set figure
/// deliberately excludes, and the interval between two samples.
const AUTO_ENFORCED_NUMERATOR: u64 = 3;
const AUTO_ENFORCED_DENOMINATOR: u64 = 4;

/// Fraction of a merely *advisory* limit — the machine's total RAM — that
/// auto-mode budgets. Half, not three quarters: that memory is shared with the
/// editor, the language servers and everything else the developer is running,
/// none of which basemind can see.
const AUTO_ADVISORY_DENOMINATOR: u64 = 2;

/// Floor for an auto-derived ceiling. A 256 MiB container would otherwise
/// compute a 192 MiB budget, which the ONNX runtime and a single large
/// tree-sitter parse can exceed on their own, turning the gate into a permanent
/// stall that never admits and never finishes. The floor deliberately allows
/// the ceiling to sit *above* a very small cgroup limit: in that regime the
/// process is going to be killed whatever it does, and a scan that runs and
/// dies is more diagnosable than one that hangs.
const AUTO_FLOOR_BYTES: u64 = 512 * BYTES_PER_MB;

/// The literal spellings `max_footprint_mb` accepts besides a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FootprintKeyword {
    /// No ceiling: the gate is a no-op. The only way to disable it, and
    /// deliberately something an operator has to spell out.
    Off,
    /// Derive a ceiling from the environment. Identical to `0`.
    Auto,
}

/// The `max_footprint_mb` setting: an explicit mebibyte ceiling, `"off"`, or
/// auto (spelled either `0` or `"auto"`).
///
/// It is an enum rather than a bare integer because `0` was already spent: it
/// used to mean "disabled", and auto is the right default, so "disabled" needed
/// a spelling of its own. Untagged, so all three forms parse from plain TOML
/// (`max_footprint_mb = 0`, `= 512`, `= "off"`) with no wrapper table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum MaxFootprint {
    /// `"off"` or `"auto"`.
    Keyword(FootprintKeyword),
    /// An explicit ceiling in mebibytes. `0` is auto, preserving the shape of
    /// every `[resources]` knob where `0` already means "pick for me".
    Mebibytes(usize),
}

impl Default for MaxFootprint {
    /// `0`, i.e. auto — and serialised back as `0`, so the schema's default
    /// stays the integer it has always been.
    fn default() -> Self {
        MaxFootprint::Mebibytes(0)
    }
}

impl MaxFootprint {
    /// Whether the operator explicitly disabled the ceiling.
    pub fn is_off(self) -> bool {
        matches!(self, MaxFootprint::Keyword(FootprintKeyword::Off))
    }

    /// Whether the ceiling is derived rather than stated.
    pub fn is_auto(self) -> bool {
        matches!(
            self,
            MaxFootprint::Keyword(FootprintKeyword::Auto) | MaxFootprint::Mebibytes(0)
        )
    }

    /// The stated ceiling in bytes, or `None` when the setting is `"off"` or auto.
    pub fn explicit_bytes(self) -> Option<u64> {
        match self {
            MaxFootprint::Mebibytes(mb) if mb > 0 => Some((mb as u64).saturating_mul(BYTES_PER_MB)),
            _ => None,
        }
    }

    /// Derive an auto ceiling from what [`crate::sysres`] can see.
    ///
    /// Pure in its inputs so the arithmetic is testable without a cgroup:
    /// `limit_bytes` is the ceiling the platform reports and `enforced` is
    /// [`crate::sysres::MemoryReading::limit_is_enforced`]. `None` in means
    /// `None` out — a platform that cannot report a limit gets no ceiling
    /// rather than a made-up one.
    pub fn auto_ceiling_bytes(limit_bytes: Option<u64>, enforced: bool) -> Option<u64> {
        let limit = limit_bytes?;
        let budget = if enforced {
            limit / AUTO_ENFORCED_DENOMINATOR * AUTO_ENFORCED_NUMERATOR
        } else {
            limit / AUTO_ADVISORY_DENOMINATOR
        };
        Some(budget.max(AUTO_FLOOR_BYTES))
    }

    /// Resolve to a ceiling in bytes, sampling the environment when the setting
    /// is auto. `None` means "no ceiling": either `"off"`, or auto on a platform
    /// that reports no limit.
    ///
    /// The sample goes through [`crate::sysres::memory_reading`], which is
    /// rate-limited, so this is safe to call per admit point.
    pub fn resolve_bytes(self) -> Option<u64> {
        if self.is_off() {
            return None;
        }
        if let Some(bytes) = self.explicit_bytes() {
            return Some(bytes);
        }
        let reading = crate::sysres::memory_reading()?;
        Self::auto_ceiling_bytes(reading.limit_bytes, reading.limit_is_enforced)
    }

    /// Resolve to whole mebibytes, which is the unit
    /// [`crate::backpressure::FootprintGate`] is constructed from. `0` is that
    /// gate's "disabled" value and is what both `"off"` and an underivable auto
    /// ceiling produce.
    pub fn resolve_mb(self) -> usize {
        match self.resolve_bytes() {
            Some(bytes) => (bytes / BYTES_PER_MB) as usize,
            None => 0,
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
        assert_eq!(cfg.max_footprint_mb, MaxFootprint::Mebibytes(0));
        assert_eq!(cfg.max_map_cache_mb, 256);
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
        assert_eq!(cfg.max_footprint_mb, MaxFootprint::Mebibytes(0));
        assert_eq!(cfg.max_map_cache_mb, 256);
        assert_eq!(cfg.document_models, DocumentModelProfile::Full);
    }

    /// `0` is the documented "unbounded" sentinel for the read-stack cache and must survive a round
    /// trip — the equivalence test drives its unbounded arm through exactly this path.
    #[test]
    fn max_map_cache_mb_accepts_zero_as_unbounded() {
        let cfg: ResourcesConfig = toml::from_str("max_map_cache_mb = 0\n").expect("parse [resources] body");
        assert_eq!(cfg.max_map_cache_mb, 0);
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

    const MB: u64 = 1024 * 1024;

    #[test]
    fn max_footprint_accepts_all_three_spellings_from_toml() {
        let auto: ResourcesConfig = toml::from_str("max_footprint_mb = 0\n").expect("parse 0");
        assert_eq!(auto.max_footprint_mb, MaxFootprint::Mebibytes(0));
        assert!(auto.max_footprint_mb.is_auto());

        let explicit: ResourcesConfig = toml::from_str("max_footprint_mb = 512\n").expect("parse 512");
        assert_eq!(explicit.max_footprint_mb, MaxFootprint::Mebibytes(512));
        assert!(!explicit.max_footprint_mb.is_auto());

        let off: ResourcesConfig = toml::from_str("max_footprint_mb = \"off\"\n").expect("parse off");
        assert_eq!(off.max_footprint_mb, MaxFootprint::Keyword(FootprintKeyword::Off));
        assert!(off.max_footprint_mb.is_off());

        let named_auto: ResourcesConfig = toml::from_str("max_footprint_mb = \"auto\"\n").expect("parse auto");
        assert_eq!(
            named_auto.max_footprint_mb,
            MaxFootprint::Keyword(FootprintKeyword::Auto)
        );
        assert!(named_auto.max_footprint_mb.is_auto());
    }

    #[test]
    fn max_footprint_rejects_an_unknown_keyword() {
        let bad = toml::from_str::<ResourcesConfig>("max_footprint_mb = \"disabled\"\n");
        assert!(bad.is_err(), "only `off` and `auto` are accepted keywords");
    }

    #[test]
    fn max_footprint_serialises_auto_back_as_the_integer_zero() {
        let json = serde_json::to_string(&MaxFootprint::default()).expect("serialize");
        assert_eq!(json, "0");
        assert_eq!(
            serde_json::to_string(&MaxFootprint::Mebibytes(256)).expect("serialize"),
            "256"
        );
        assert_eq!(
            serde_json::to_string(&MaxFootprint::Keyword(FootprintKeyword::Off)).expect("serialize"),
            "\"off\""
        );
    }

    #[test]
    fn explicit_bytes_is_only_set_for_a_positive_mebibyte_count() {
        assert_eq!(MaxFootprint::Mebibytes(512).explicit_bytes(), Some(512 * MB));
        assert_eq!(MaxFootprint::Mebibytes(0).explicit_bytes(), None);
        assert_eq!(MaxFootprint::Keyword(FootprintKeyword::Off).explicit_bytes(), None);
        assert_eq!(MaxFootprint::Keyword(FootprintKeyword::Auto).explicit_bytes(), None);
    }

    #[test]
    fn auto_ceiling_takes_three_quarters_of_an_enforced_cgroup_limit() {
        // The reporter's environment: a 2 GiB `MemoryMax`.
        assert_eq!(MaxFootprint::auto_ceiling_bytes(Some(2048 * MB), true), Some(1536 * MB));
        assert_eq!(MaxFootprint::auto_ceiling_bytes(Some(8192 * MB), true), Some(6144 * MB));
    }

    #[test]
    fn auto_ceiling_takes_half_of_a_machine_wide_limit() {
        assert_eq!(
            MaxFootprint::auto_ceiling_bytes(Some(16 * 1024 * MB), false),
            Some(8 * 1024 * MB)
        );
    }

    #[test]
    fn auto_ceiling_floor_applies_to_a_small_container() {
        // 256 MiB cgroup: three quarters is 192 MiB, which the floor lifts to 512 MiB.
        assert_eq!(MaxFootprint::auto_ceiling_bytes(Some(256 * MB), true), Some(512 * MB));
        // 1 GiB of machine RAM: half is 512 MiB, exactly the floor.
        assert_eq!(MaxFootprint::auto_ceiling_bytes(Some(1024 * MB), false), Some(512 * MB));
        // 800 MiB of machine RAM: half is 400 MiB, lifted to the floor.
        assert_eq!(MaxFootprint::auto_ceiling_bytes(Some(800 * MB), false), Some(512 * MB));
    }

    #[test]
    fn auto_ceiling_is_absent_when_no_limit_can_be_detected() {
        assert_eq!(MaxFootprint::auto_ceiling_bytes(None, true), None);
        assert_eq!(MaxFootprint::auto_ceiling_bytes(None, false), None);
    }

    #[test]
    fn auto_ceiling_never_overflows_on_a_sentinel_sized_limit() {
        let ceiling = MaxFootprint::auto_ceiling_bytes(Some(u64::MAX), true).expect("a ceiling");
        assert!(ceiling >= 512 * MB && ceiling < u64::MAX);
    }

    #[test]
    fn resolve_bytes_honours_off_and_an_explicit_ceiling_without_sampling() {
        assert_eq!(MaxFootprint::Keyword(FootprintKeyword::Off).resolve_bytes(), None);
        assert_eq!(MaxFootprint::Keyword(FootprintKeyword::Off).resolve_mb(), 0);
        assert_eq!(MaxFootprint::Mebibytes(384).resolve_bytes(), Some(384 * MB));
        assert_eq!(MaxFootprint::Mebibytes(384).resolve_mb(), 384);
    }

    /// On any platform `sysres` supports, auto must produce a real ceiling — the whole point
    /// of F2 is that the default is no longer inert. Elsewhere it degrades to no ceiling.
    #[test]
    fn resolve_mb_auto_yields_a_ceiling_on_a_supported_platform() {
        let resolved = MaxFootprint::default().resolve_mb();
        if cfg!(any(target_os = "linux", target_os = "macos", windows)) {
            assert!(
                resolved >= 512,
                "auto must clear the 512 MiB floor on a supported platform, got {resolved}"
            );
        } else {
            assert_eq!(resolved, 0, "an unsupported platform reports no ceiling");
        }
    }

    #[test]
    fn effective_embed_threads_prefers_resources_then_deprecated_alias() {
        let cfg = ResourcesConfig {
            embed_threads: 4,
            ..ResourcesConfig::default()
        };
        assert_eq!(cfg.effective_embed_threads(8), 4);
        let cfg = ResourcesConfig::default();
        assert_eq!(cfg.effective_embed_threads(8), 8);
        assert_eq!(cfg.effective_embed_threads(0), 0);
    }
}
