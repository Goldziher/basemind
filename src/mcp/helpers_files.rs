//! `run_list_files` + `run_find_files` — kept in their own file so `tools.rs` and `helpers.rs`
//! stay under the 1000-line cap as the MCP surface grows. Both enumerate indexed paths and share the
//! same limit/cursor/token-budget pagination shape; `find_files` additionally scores each candidate
//! with `nucleo-matcher`, and with [`within_one_edit`] for a query that names nothing as typed,
//! before paginating.

use std::sync::atomic::Ordering;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::helpers_filters::{Filters, PathFilter, Tally};
use super::types::{FilesResponse, FindFilesParams, ListFilesParams};
use crate::path::RelPath;

/// The response for a cursor minted on another in-RAM snapshot: restart from the top.
fn invalidated(state: &ServerState, limit_clamped: bool) -> Result<CallToolResult, McpError> {
    super::helpers::json_result(&FilesResponse {
        total: 0,
        limit_clamped,
        budgeted: false,
        files: Vec::new(),
        next_cursor: None,
        cursor_invalidated: true,
        notice: state.lifecycle_notice(),
    })
}

/// Body of `find_files` without a query: enumerate indexed paths with optional substring
/// (`path_contains`), prefix and `language` filters, then paginate.
pub(super) async fn run_list_files(state: &ServerState, params: ListFilesParams) -> Result<CallToolResult, McpError> {
    state.await_cache_ready().await;
    let (limit, limit_clamped) = super::tools::effective_list_limit(params.limit);
    let generation = state.shared.cache_generation.load(Ordering::Relaxed);

    let skip = match params.cursor.as_ref() {
        Some(c) => {
            let (offset, snapshot_id) = c.decode_in_memory()?;
            if snapshot_id != generation {
                return invalidated(state, limit_clamped);
            }
            offset as usize
        }
        None => 0,
    };
    let store = state.shared.store.read().await;
    let cache = state.shared.cache.load_full();

    let path_finder = params
        .path_contains
        .as_ref()
        .map(|n| memchr::memmem::Finder::new(n.as_bytes()));
    let prefix_filter = params.path_prefix.as_deref().map(str::as_bytes);
    let lang_filter = params.language.as_deref();

    // Code map first, then the document tier (markdown, config, data, PDFs ...) in sorted order:
    // a file listing covers every indexed path, not just the code-mapped ones.
    // The index map iterates in hash order, which differs per process; a cursor is an offset into
    // this listing, so the order must be stable across calls.
    let mut code_sorted: Vec<_> = store.index.files.iter().collect();
    code_sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let code = code_sorted.into_iter().map(|(p, e)| (p, e.language.clone()));
    let doc_paths = std::sync::Arc::clone(&cache.doc_paths);
    let docs = doc_paths.iter().map(|p| (p, language_of_doc(p)));

    let mut files: Vec<RelPath> = Vec::with_capacity(limit.min(256));
    let mut total: usize = 0;
    let mut seen: usize = 0;
    for (p, language) in code.chain(docs) {
        let path_ok = path_finder.as_ref().is_none_or(|f| f.find(p.as_bytes()).is_some())
            && prefix_filter.is_none_or(|prefix| p.as_bytes().starts_with(prefix));
        let lang_ok = lang_filter.is_none_or(|l| language == l);
        if !(path_ok && lang_ok) {
            continue;
        }
        if seen < skip {
            seen += 1;
            continue;
        }
        seen += 1;
        total += 1;
        if files.len() < limit {
            files.push(p.clone());
        }
    }
    if total == 0 && skip == 0 {
        let filters = Filters {
            tool: "find_files",
            path: params.path_prefix.as_deref().map(PathFilter::Prefix),
            language: lang_filter,
        };
        let listed = || {
            let mut tally = Tally::default();
            for (path, entry) in store.index.files.iter() {
                tally.see(&filters, path.as_bytes(), &entry.language);
            }
            for path in doc_paths.iter() {
                tally.see(&filters, path.as_bytes(), &language_of_doc(path));
            }
            tally
        };
        if let Some(refusal) = selects_no_file(state, &filters, listed, None) {
            return Err(refusal);
        }
    }
    respond(state, files, total, skip, limit_clamped, generation, params.max_tokens)
}

/// The page of `files` (`total` of them from `skip` on), held to `max_tokens`.
fn respond(
    state: &ServerState,
    files: Vec<RelPath>,
    total: usize,
    skip: usize,
    limit_clamped: bool,
    generation: u32,
    max_tokens: Option<u32>,
) -> Result<CallToolResult, McpError> {
    let budget = super::budget::apply_budget(files, max_tokens);
    let files = budget.items;
    let next_cursor =
        (total > files.len()).then(|| super::cursor::Cursor::encode_in_memory((skip + files.len()) as u64, generation));
    super::helpers::json_result(&FilesResponse {
        total,
        limit_clamped,
        budgeted: budget.budgeted,
        files,
        next_cursor,
        cursor_invalidated: false,
        notice: state.lifecycle_notice(),
    })
}

/// Body of `find_files` with a query: fuzzy subsequence match over indexed paths (fzf/fd-style),
/// ranked by `nucleo-matcher` score, with optional `path_prefix` / `language`
/// pre-filters. A query no path contains as typed may be a typo: files it names up to one edit
/// ([`within_one_edit`]) rank first.
///
/// Sourced from the `MapCache` file view (already sorted, in-RAM, no store lock needed) — the
/// `await_cache_ready()` call above guarantees it is populated before this runs. Paths that
/// aren't valid UTF-8 are skipped — `nucleo-matcher` scores `str`, not raw bytes.
pub(super) async fn run_find_files(state: &ServerState, params: FindFilesParams) -> Result<CallToolResult, McpError> {
    state.await_cache_ready().await;
    let (limit, limit_clamped) = super::tools::effective_list_limit(params.limit);
    let generation = state.shared.cache_generation.load(Ordering::Relaxed);

    let skip = match params.cursor.as_ref() {
        Some(c) => {
            let (offset, snapshot_id) = c.decode_in_memory()?;
            if snapshot_id != generation {
                return invalidated(state, limit_clamped);
            }
            offset as usize
        }
        None => 0,
    };

    let cache = state.shared.cache.load_full();
    let prefix_filter = params.path_prefix.as_deref();
    let lang_filter = params.language.as_deref();

    // Language and path are all the fuzzy-file search needs, and both live in the symbol-free file
    // view — so this whole scan reads no outline and touches no blob. The document tier (markdown,
    // JSON, YAML, PDFs ...) is not code-mapped, but a file finder must cover every indexed path.
    let code = cache.file_metas().map(|(p, meta)| (p, meta.language.to_string()));
    let docs = cache.doc_paths.iter().map(|p| (p, language_of_doc(p)));
    let candidates: Vec<&RelPath> = code
        .chain(docs)
        .filter(|(p, language)| {
            lang_filter.is_none_or(|l| *language == l)
                && prefix_filter.is_none_or(|pre| p.as_bytes().starts_with(pre.as_bytes()))
        })
        .map(|(p, _)| p)
        .collect();

    if candidates.is_empty() {
        let filters = Filters {
            tool: "find_files",
            path: prefix_filter.map(PathFilter::Prefix),
            language: lang_filter,
        };
        let listed = || {
            let mut tally = Tally::default();
            for (path, meta) in cache.file_metas() {
                tally.see(&filters, path.as_bytes(), &meta.language);
            }
            for path in cache.doc_paths.iter() {
                tally.see(&filters, path.as_bytes(), &language_of_doc(path));
            }
            tally
        };
        if let Some(refusal) = selects_no_file(state, &filters, listed, None) {
            return Err(refusal);
        }
    }
    let mut scored = score(&params.query, &candidates);
    rank_and_cut(&mut scored);

    // ~keep: `total` mirrors `list_files`'s convention: matches remaining from `skip` onward
    // ~keep: (not the grand total across all pages), so `total > limit` / `total > files.len()`
    // ~keep: read the same way.
    let total = scored.len().saturating_sub(skip);
    let page: Vec<RelPath> = scored
        .into_iter()
        .skip(skip)
        .take(limit)
        .map(|(_, path)| path.clone())
        .collect();
    respond(state, page, total, skip, limit_clamped, generation, params.max_tokens)
}

/// Every candidate `query` matches, with its score. A fuzzy subsequence match scores what nucleo
/// gives it. When no candidate holds `query` as typed (case aside), a file the query names up to one
/// edit outranks every fuzzy match: a transposed or dropped letter otherwise matches nothing, or
/// only paths that happen to hold the letters in order.
fn score<'a>(query: &str, candidates: &[&'a RelPath]) -> Vec<(u32, &'a RelPath)> {
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let mut utf32_buf: Vec<char> = Vec::new();
    let needle = query.to_ascii_lowercase();
    let finder = memchr::memmem::Finder::new(needle.as_bytes());
    let mut lowered: Vec<u8> = Vec::new();
    let mut literal = false;
    let mut scored: Vec<(u32, &RelPath)> = Vec::new();
    for &path in candidates {
        let Some(path_str) = path.as_str() else {
            continue;
        };
        if !literal {
            lowered.clear();
            lowered.extend(path_str.bytes().map(|b| b.to_ascii_lowercase()));
            literal = finder.find(&lowered).is_some();
        }
        if let Some(score) = pattern.score(Utf32Str::new(path_str, &mut utf32_buf), &mut matcher) {
            scored.push((score, path));
        }
    }
    if literal || needle.chars().count() < TYPO_MIN_CHARS {
        return scored;
    }
    let typo_score = scored
        .iter()
        .map(|(score, _)| *score)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let query: Vec<char> = needle.chars().collect();
    for &path in candidates {
        if path.as_str().is_some_and(|path| names_with_typo(&query, path)) {
            match scored.iter_mut().find(|(_, scored)| *scored == path) {
                Some(entry) => entry.0 = typo_score,
                None => scored.push((typo_score, path)),
            }
        }
    }
    scored
}

/// Shortest query a typo is looked for in: a shorter one is one edit away from too many names.
const TYPO_MIN_CHARS: usize = 4;

/// Whether `query` (lowercase) names the file at `path` up to one edit: against the file name, the
/// file name without its extension, or, for a query holding `/`, the path's last segments.
fn names_with_typo(query: &[char], path: &str) -> bool {
    let lowered = path.to_lowercase();
    let segments = query.iter().filter(|c| **c == '/').count() + 1;
    let tail = lowered.rsplitn(segments + 1, '/').take(segments).collect::<Vec<_>>();
    let tail: String = tail.into_iter().rev().collect::<Vec<_>>().join("/");
    let name = lowered.rsplit('/').next().unwrap_or(&lowered);
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    [tail.as_str(), name, stem]
        .iter()
        .any(|target| within_one_edit(query, &target.chars().collect::<Vec<_>>()))
}

/// Whether `a` turns into `b` with at most one edit: two adjacent characters swapped, or one
/// character inserted, dropped or replaced.
fn within_one_edit(a: &[char], b: &[char]) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if long.len() - short.len() > 1 {
        return false;
    }
    let Some(i) = short.iter().zip(long).position(|(x, y)| x != y) else {
        return true;
    };
    if short.len() == long.len() {
        let swapped = i + 1 < short.len() && short[i] == long[i + 1] && short[i + 1] == long[i];
        let rest = if swapped { i + 2 } else { i + 1 };
        short[rest..] == long[rest..]
    } else {
        short[i..] == long[i + 1..]
    }
}

/// A candidate must score at least this fraction of the best candidate's score to be returned.
/// nucleo accepts any subsequence, so a short query matches thousands of paths at a score a small
/// fraction of the best; returning them only fills the page with noise.
///
/// Calibrated with the `find` eval (`benchmarks/eval/gen_find.py`, 90 tasks over code and
/// documents, limit 10): precision rises monotonically with the floor (0.44 at 50, 0.62 at 85,
/// 0.70 at 95) while recall, hit@1 and MRR stay flat until 95; at 100 (ties with the top score
/// only) one correct hit is lost. 85 keeps a ten-point margin from that cliff.
const RELATIVE_SCORE_FLOOR_PCT: u32 = 85;

/// Order candidates by descending score (path ascending on ties, so pages are deterministic) and
/// drop everything scoring below [`RELATIVE_SCORE_FLOOR_PCT`] percent of the top score.
fn rank_and_cut(scored: &mut Vec<(u32, &RelPath)>) {
    // ~keep: descending score, stable tie-break on path (ascending) so identically-scored
    // ~keep: entries have a deterministic order across calls/pages.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    if let Some(&(top, _)) = scored.first() {
        let floor = (u64::from(top) * u64::from(RELATIVE_SCORE_FLOOR_PCT) / 100) as u32;
        let keep = scored.partition_point(|(s, _)| *s >= floor);
        scored.truncate(keep);
    }
}

/// [`doc_language`] of an indexed document path; `document` for one that is not UTF-8.
fn language_of_doc(path: &crate::path::RelPath) -> String {
    path.as_str().map_or_else(|| "document".to_string(), doc_language)
}

/// Whether `notice` says files may be missing from the listing, so that "no file passes the
/// filter" proves nothing yet: an index that is warming, building, rebuilding or empty. Those are
/// the notices that ask for a retry. A rescan in progress and capped reference projections leave
/// the file listing whole, and the second lasts for the session.
fn listing_incomplete(notice: &super::types::LifecycleNotice) -> bool {
    notice.retry
}

/// The refusal for filters that selected none of the files a tool lists, or `None` when there is
/// nothing to refuse: no filter, an empty index, or an index still being built, whose page carries
/// the lifecycle `notice` instead. `listed` tallies the files the tool lists, from the source it
/// lists them from; it runs only on this path, after the listing itself came back empty.
pub(super) fn selects_no_file(
    state: &ServerState,
    filters: &Filters<'_>,
    listed: impl FnOnce() -> Tally,
    unlisted_documents: Option<&dyn Fn() -> Tally>,
) -> Option<McpError> {
    if filters.is_empty() || state.lifecycle_notice().as_ref().is_some_and(listing_incomplete) {
        return None;
    }
    let documents = unlisted_documents.map(|tally| tally());
    filters.refusal(&listed(), documents.as_ref())
}

/// [`selects_no_file`] for `code_grep`, which searches the code map and not the document tier.
pub(super) fn grep_selects_no_file(
    state: &ServerState,
    cache: &super::MapCache,
    filters: &Filters<'_>,
) -> Option<McpError> {
    let listed = || {
        let mut tally = Tally::default();
        for (path, meta) in cache.file_metas() {
            tally.see(filters, path.as_bytes(), &meta.language);
        }
        tally
    };
    let documents = || {
        let mut tally = Tally::default();
        for path in cache.doc_paths.iter() {
            tally.see(filters, path.as_bytes(), &language_of_doc(path));
        }
        tally
    };
    selects_no_file(state, filters, listed, Some(&documents))
}

/// Language label for a document-tier path: the grammar name when one resolves (`markdown`,
/// `json`, `yaml` ...), else the lowercased extension (`pdf`, `docx`), else `document`.
pub(super) fn doc_language(path: &str) -> String {
    if let Some(l) = crate::lang::detect(std::path::Path::new(path)) {
        return l.to_string();
    }
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map_or_else(|| "document".to_string(), str::to_ascii_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::Lifecycle;
    use crate::mcp::types::LifecycleNotice;

    #[test]
    fn a_filter_refusal_is_held_back_only_while_files_may_be_missing() {
        let of = |state| LifecycleNotice::for_state(state).expect("notice");
        for incomplete in [
            of(Lifecycle::WarmingUp),
            of(Lifecycle::BuildingIndex),
            LifecycleNotice::index_rebuilding(),
            LifecycleNotice::empty_index(),
        ] {
            assert!(listing_incomplete(&incomplete), "{}", incomplete.state);
        }
        for whole in [of(Lifecycle::Rescanning), LifecycleNotice::projections_capped()] {
            assert!(!listing_incomplete(&whole), "{}", whole.state);
        }
    }

    #[test]
    fn cut_drops_far_below_top_and_orders_deterministically() {
        let [a, b, z, low] = ["a", "b", "z", "low"].map(RelPath::from);
        let mut v = vec![(100, &b), (100, &a), (85, &z), (84, &low)];
        rank_and_cut(&mut v);
        let paths: Vec<_> = v.iter().map(|(_, path)| path.to_string()).collect();
        assert_eq!(paths, ["a", "b", "z"], "85 is kept (>= floor), 84 is cut");
    }

    #[test]
    fn cut_keeps_everything_when_scores_are_close_and_handles_empty() {
        let [a, b] = ["a", "b"].map(RelPath::from);
        let mut v = vec![(10, &a), (9, &b)];
        rank_and_cut(&mut v);
        assert_eq!(v.len(), 2);
        let mut e: Vec<(u32, &RelPath)> = Vec::new();
        rank_and_cut(&mut e);
        assert!(e.is_empty());
    }

    fn chars(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    #[test]
    fn one_edit_is_a_swap_an_insertion_a_deletion_or_a_replacement() {
        for (typed, meant) in [
            ("buidl", "build"),
            ("sceurity", "security"),
            ("baner", "banner"),
            ("bannner", "banner"),
            ("bamner", "banner"),
            ("build", "build"),
        ] {
            assert!(within_one_edit(&chars(typed), &chars(meant)), "{typed} -> {meant}");
        }
        for (typed, meant) in [("bdiul", "build"), ("bnr", "banner"), ("tset", "testing")] {
            assert!(!within_one_edit(&chars(typed), &chars(meant)), "{typed} -> {meant}");
        }
    }

    #[test]
    fn a_typo_names_a_file_by_its_name_its_stem_or_its_last_segments() {
        assert!(names_with_typo(
            &chars("blocking_cehck"),
            "tokio/src/util/blocking_check.rs"
        ));
        assert!(names_with_typo(&chars("sceurity"), "SECURITY.md"));
        assert!(names_with_typo(&chars("favcion-32"), "website/public/favicon-32.png"));
        assert!(names_with_typo(
            &chars("util/blocking_chekc.rs"),
            "tokio/src/util/blocking_check.rs"
        ));
        assert!(!names_with_typo(&chars("blocking_cehck"), "tokio/src/util/blocking.rs"));
    }

    #[test]
    fn a_typo_outranks_paths_that_only_hold_its_letters_in_order() {
        let paths = [
            "SECURITY.md",
            "tokio/src/runtime/scheduler/inject/synced.rs",
            "src/lib.rs",
        ]
        .map(RelPath::from);
        let candidates: Vec<&RelPath> = paths.iter().collect();
        let mut scored = score("SCEURITY", &candidates);
        rank_and_cut(&mut scored);
        assert_eq!(scored[0].1.to_string(), "SECURITY.md", "{scored:?}");

        // `synced` is one edit from `syncer`, but a path holds it as typed: not a typo.
        let paths = ["a/synced.rs", "b/syncer.rs"].map(RelPath::from);
        let candidates: Vec<&RelPath> = paths.iter().collect();
        let typed_right: Vec<String> = score("synced", &candidates)
            .iter()
            .map(|(_, path)| path.to_string())
            .collect();
        assert_eq!(typed_right, ["a/synced.rs"]);
    }

    #[test]
    fn doc_language_labels() {
        assert_eq!(doc_language("docs/README.md"), "markdown");
        assert_eq!(doc_language("a/b.pdf"), "pdf");
        assert_eq!(doc_language("LICENSEFILE"), "document");
    }
}
