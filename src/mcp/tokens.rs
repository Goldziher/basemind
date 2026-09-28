//! Token counting for compression reports (and, later, budgeting).
//!
//! Real HF-tokenizer counts (o200k / gpt-4o, via xberg) when the `tokenizer`
//! feature is enabled (pulled in unconditionally by `documents`, which needs it
//! for the same reason plus more); a `bytes/4` heuristic otherwise. The real
//! path downloads the tokenizer from HF on first use and caches it, and falls
//! back to a word estimate offline — acceptable for the explicit `compress` op
//! and `basemind admin tokens`, NOT for any per-call hot path.

/// Count the tokens in `text`.
#[cfg(feature = "tokenizer")]
pub(crate) fn count_tokens(text: &str) -> u64 {
    xberg::chunking::count_tokens(text, None) as u64
}

/// `bytes / 4` fallback when no tokenizer is compiled in.
#[cfg(not(feature = "tokenizer"))]
pub(crate) fn count_tokens(text: &str) -> u64 {
    (text.len() as u64) / 4
}

/// `true` when [`count_tokens`] uses a real tokenizer (the `tokenizer` feature),
/// `false` when it uses the `bytes/4` heuristic.
pub(crate) const TOKENS_ARE_COUNTED: bool = cfg!(feature = "tokenizer");

#[cfg(test)]
mod tests {
    use super::*;

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
