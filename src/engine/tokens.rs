//! Token counting for savings telemetry, budgets, `admin eval` and `basemind admin tokens`.
//!
//! Real HF-tokenizer counts (o200k / gpt-4o, via xberg) when the `tokenizer`
//! feature is enabled (pulled in unconditionally by `documents`, which needs it
//! for the same reason plus more); a `bytes/4` heuristic otherwise. The real
//! path downloads the tokenizer from HF on first use and caches it, and falls
//! back to a word estimate offline — acceptable for `basemind admin tokens`, NOT
//! for any per-call hot path.

/// `bytes / 4` token estimate, saturating. The byte-only fallback used wherever the full text
/// is NOT in hand — only a byte length. Shared by the budget helper ([`super::budget`]) and the
/// telemetry estimator, so both use the exact same bytes→token factor.
pub(crate) fn bytes_to_tokens(bytes: u64) -> u64 {
    bytes / 4
}

/// Count the tokens in `text`.
#[cfg(feature = "tokenizer")]
pub fn count_tokens(text: &str) -> u64 {
    xberg::chunking::count_tokens(text, None) as u64
}

/// `bytes / 4` fallback when no tokenizer is compiled in.
#[cfg(not(feature = "tokenizer"))]
pub fn count_tokens(text: &str) -> u64 {
    (text.len() as u64) / 4
}

/// `true` when [`count_tokens`] uses a real tokenizer (the `tokenizer` feature),
/// `false` when it uses the `bytes/4` heuristic.
pub(crate) const TOKENS_ARE_COUNTED: bool = cfg!(feature = "tokenizer");

/// A string whose o200k count differs from the offline word estimate (2), so a count of it tells
/// the real tokenizer from the fallback. `benchmarks/eval/run_gate.sh` checks the same pair.
#[cfg(feature = "tokenizer")]
pub(crate) const O200K_PROBE: &str = "Hello, world!";
/// The o200k token count of [`O200K_PROBE`].
#[cfg(feature = "tokenizer")]
pub(crate) const O200K_PROBE_TOKENS: u64 = 4;

/// `true` when [`count_tokens`] really counts o200k: the `tokenizer` feature is on and the
/// tokenizer loaded, which [`O200K_PROBE`] proves because the word-estimate fallback (an offline
/// first use) counts it differently. Probed once.
pub(crate) fn o200k_counted() -> bool {
    #[cfg(feature = "tokenizer")]
    {
        static COUNTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *COUNTED.get_or_init(|| count_tokens(O200K_PROBE) == O200K_PROBE_TOKENS)
    }
    #[cfg(not(feature = "tokenizer"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The word estimate must not equal the probe's o200k count, or the probe could not tell them apart.
    #[cfg(feature = "tokenizer")]
    #[test]
    fn probe_is_not_what_the_word_estimate_counts() {
        assert_ne!(O200K_PROBE.split_whitespace().count() as u64, O200K_PROBE_TOKENS);
    }

    /// With the tokenizer cached or downloadable the probe counts as o200k.
    #[cfg(feature = "tokenizer")]
    #[test]
    fn probe_counts_as_o200k_when_the_tokenizer_loads() {
        assert_eq!(count_tokens(O200K_PROBE), O200K_PROBE_TOKENS);
        assert!(o200k_counted());
    }

    #[cfg(not(feature = "tokenizer"))]
    #[test]
    fn no_feature_is_never_o200k() {
        assert!(!o200k_counted());
    }

    /// Under the heuristic path (no `tokenizer`), the count is exactly `bytes / 4`.
    #[cfg(not(feature = "tokenizer"))]
    #[test]
    fn fallback_count_is_bytes_over_four() {
        let text = "a".repeat(400);
        assert_eq!(count_tokens(&text), 100);
        assert_eq!(count_tokens("aaaa"), 1);
        assert_eq!(count_tokens(""), 0);
    }

    /// `TOKENS_ARE_COUNTED` mirrors the `tokenizer` feature. Read through a runtime
    /// binding so the comparison is not a compile-time constant (which clippy flags).
    #[test]
    fn tokens_are_counted_tracks_documents_feature() {
        let counted = TOKENS_ARE_COUNTED;
        assert_eq!(counted, cfg!(feature = "tokenizer"));
    }
}
