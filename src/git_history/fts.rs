//! Full-text search over the git-history index: tokenization + the `gh_term_to_ords` query path.
//!
//! Terms are indexed per commit into two fields — author identity (name + email) and message
//! (summary + body) — so a query can scope to authors, to messages, or search both. Each
//! `(field, term)` maps to a posting list of commit ordinals, reusing the same newest-first
//! delta-varint machinery ([`super::encoding::encode_ords`]) as the path index. A multi-term query
//! is an AND: a commit matches only if every query term appears (in the scoped field set).

use ahash::{AHashMap, AHashSet};

use super::{GitHistoryIndex, encoding, keys};
use crate::git::CommitInfo;

/// Field tag stored as the leading byte of a `gh_term_to_ords` key. Stable, append-only — the byte
/// is persisted, so never reorder; new fields extend the tail.
pub const FIELD_AUTHOR: u8 = 0;
pub const FIELD_MESSAGE: u8 = 1;

/// Longest token (in bytes) the tokenizer keeps. Guards against a pathological non-whitespace blob
/// (minified data, a base64 dump pasted into a commit message) bloating the term index; real search
/// terms are far shorter. Matches the spirit of the index module's 64 KiB key ceiling.
const MAX_TERM_LEN: usize = 128;

/// Which field(s) a search covers. `All` unions the author and message posting lists per term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtsScope {
    Author,
    Message,
    All,
}

impl FtsScope {
    /// Parse the MCP `field` param. Unknown / absent → `All` (search everything).
    pub fn parse(field: Option<&str>) -> Self {
        match field.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("author") => FtsScope::Author,
            Some("message") | Some("summary") | Some("body") => FtsScope::Message,
            _ => FtsScope::All,
        }
    }

    /// The field bytes this scope reads a term's postings from.
    fn fields(self) -> &'static [u8] {
        match self {
            FtsScope::Author => &[FIELD_AUTHOR],
            FtsScope::Message => &[FIELD_MESSAGE],
            FtsScope::All => &[FIELD_AUTHOR, FIELD_MESSAGE],
        }
    }
}

/// Split `text` into lowercased alphanumeric tokens, inserting each distinct token into `out`.
/// Runs of non-alphanumeric characters are separators, so `fix(api): cache` → {`fix`, `api`,
/// `cache`} and `jane@example.com` → {`jane`, `example`, `com`}. Tokens longer than
/// [`MAX_TERM_LEN`] are dropped. `AHashSet` dedups within a field so a repeated word writes one
/// posting edge per commit.
pub fn tokenize(text: &str, out: &mut AHashSet<String>) {
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            flush_token(&mut current, out);
        }
    }
    flush_token(&mut current, out);
}

fn flush_token(current: &mut String, out: &mut AHashSet<String>) {
    if current.is_empty() {
        return;
    }
    if current.len() <= MAX_TERM_LEN {
        out.insert(std::mem::take(current));
    } else {
        current.clear();
    }
}

/// Tokenize one commit's searchable fields and push each `(field, term)` edge onto `postings`,
/// keyed by the full `gh_term_to_ords` key bytes. Author name + email tokenize into the author
/// field; summary + body into the message field. Called once per commit from the builder fold.
pub fn index_commit_terms(
    postings: &mut AHashMap<Vec<u8>, Vec<u32>>,
    ord: u32,
    author: &str,
    author_email: &str,
    summary: &str,
    body: &str,
) {
    let mut author_terms = AHashSet::new();
    tokenize(author, &mut author_terms);
    tokenize(author_email, &mut author_terms);
    for term in &author_terms {
        postings
            .entry(keys::term_key(FIELD_AUTHOR, term.as_bytes()))
            .or_default()
            .push(ord);
    }

    let mut message_terms = AHashSet::new();
    tokenize(summary, &mut message_terms);
    tokenize(body, &mut message_terms);
    for term in &message_terms {
        postings
            .entry(keys::term_key(FIELD_MESSAGE, term.as_bytes()))
            .or_default()
            .push(ord);
    }
}

/// Does `info` match the already-tokenized `query_terms` under `scope`, by the same tokenized-AND
/// semantics the index uses? Reused by the bounded live-walk fallback (when the git-history index
/// isn't fresh) so its results are consistent with the indexed path — modulo whichever fields the
/// live `CommitInfo` populated (e.g. a summary-only live record contributes no body terms). Takes
/// pre-tokenized query terms so the caller tokenizes the (loop-invariant) query ONCE, not per
/// commit across the whole window.
pub fn commit_matches_terms(
    info: &CommitInfo,
    query_terms: &AHashSet<String>,
    scope: FtsScope,
) -> bool {
    if query_terms.is_empty() {
        return false;
    }
    let mut have: AHashSet<String> = AHashSet::new();
    if matches!(scope, FtsScope::Author | FtsScope::All) {
        tokenize(&info.author, &mut have);
        tokenize(&info.author_email, &mut have);
    }
    if matches!(scope, FtsScope::Message | FtsScope::All) {
        tokenize(&info.summary, &mut have);
        tokenize(&info.body, &mut have);
    }
    query_terms.iter().all(|term| have.contains(term))
}

/// Convenience wrapper: tokenize `query` then delegate to [`commit_matches_terms`]. Prefer the
/// terms variant in a per-commit loop to avoid re-tokenizing the invariant query.
pub fn commit_matches(info: &CommitInfo, query: &str, scope: FtsScope) -> bool {
    let mut query_terms: AHashSet<String> = AHashSet::new();
    tokenize(query, &mut query_terms);
    commit_matches_terms(info, &query_terms, scope)
}

impl GitHistoryIndex {
    /// Full-text search over indexed commits. Tokenizes `query` the same way the index was built,
    /// then returns the commits (newest-first) that contain EVERY query term in the scoped field
    /// set — after skipping `skip` and taking at most `take`. Result [`CommitInfo`]s carry the full
    /// body (read from `gh_commit_text_by_ord`). An empty query, or one that tokenizes to nothing,
    /// returns no results.
    ///
    /// Note: this rebuilds the full term intersection on every call (offset pagination, no scan
    /// cap), so a page-K request is O(|matching set|), not O(skip + take). Fine for typical
    /// queries; a very common single token over a huge repo pays for the whole posting list per
    /// page. Acceptable for now — revisit with a lazy/most-selective-term iterator if it bites.
    pub fn search_commits(
        &self,
        query: &str,
        scope: FtsScope,
        skip: usize,
        take: usize,
    ) -> Vec<CommitInfo> {
        let mut query_terms: AHashSet<String> = AHashSet::new();
        tokenize(query, &mut query_terms);
        if query_terms.is_empty() || take == 0 {
            return Vec::new();
        }

        // AND across terms: start from the first term's ordinal set, then intersect each remaining
        // term's set into it. Bail the moment the running intersection empties.
        let mut matching: Option<AHashSet<u32>> = None;
        for term in &query_terms {
            let ords = self.ords_for_term(term, scope);
            matching = Some(match matching {
                None => ords,
                Some(acc) => {
                    let mut acc = acc;
                    acc.retain(|ord| ords.contains(ord));
                    acc
                }
            });
            if matching.as_ref().is_some_and(|set| set.is_empty()) {
                return Vec::new();
            }
        }
        let Some(matching) = matching else {
            return Vec::new();
        };

        // Newest-first (ordinals are assigned oldest→newest, so descending == most recent first).
        let mut ords: Vec<u32> = matching.into_iter().collect();
        ords.sort_unstable_by(|a, b| b.cmp(a));

        let mut cache = AHashMap::new();
        ords.into_iter()
            .skip(skip)
            .take(take)
            .filter_map(|ord| self.commit_meta(ord, false).map(|meta| (ord, meta)))
            .map(|(ord, meta)| {
                let mut info = self.meta_to_info(meta, &mut cache);
                info.body = self.commit_text(ord).unwrap_or_default();
                info
            })
            .collect()
    }

    /// Union of a term's posting lists across the scope's fields, as an ordinal set.
    fn ords_for_term(&self, term: &str, scope: FtsScope) -> AHashSet<u32> {
        let mut out = AHashSet::new();
        for &field in scope.fields() {
            if let Some(bytes) = self.term_posting_bytes(&keys::term_key(field, term.as_bytes())) {
                out.extend(encoding::decode_ords(&bytes));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(text: &str) -> Vec<String> {
        let mut set = AHashSet::new();
        tokenize(text, &mut set);
        let mut v: Vec<String> = set.into_iter().collect();
        v.sort();
        v
    }

    #[test]
    fn tokenize_lowercases_and_splits_on_non_alphanumeric() {
        assert_eq!(toks("fix(api): Cache"), vec!["api", "cache", "fix"]);
    }

    #[test]
    fn tokenize_email_splits_into_local_and_host_parts() {
        assert_eq!(
            toks("Jane.Doe@Example.com"),
            vec!["com", "doe", "example", "jane"]
        );
    }

    #[test]
    fn tokenize_dedups_repeats() {
        assert_eq!(toks("fix fix FIX"), vec!["fix"]);
    }

    #[test]
    fn tokenize_drops_oversized_blob_but_keeps_neighbors() {
        let blob = "x".repeat(MAX_TERM_LEN + 1);
        let text = format!("keep {blob} tail");
        assert_eq!(toks(&text), vec!["keep", "tail"]);
    }

    #[test]
    fn empty_and_punctuation_only_tokenize_to_nothing() {
        assert!(toks("").is_empty());
        assert!(toks("   -- .. //").is_empty());
    }

    #[test]
    fn scope_parse_maps_aliases() {
        assert_eq!(FtsScope::parse(Some("author")), FtsScope::Author);
        assert_eq!(FtsScope::parse(Some("Message")), FtsScope::Message);
        assert_eq!(FtsScope::parse(Some("body")), FtsScope::Message);
        assert_eq!(FtsScope::parse(None), FtsScope::All);
        assert_eq!(FtsScope::parse(Some("whatever")), FtsScope::All);
    }
}
