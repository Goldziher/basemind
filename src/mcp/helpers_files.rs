//! `run_list_files` + `run_find_files` — kept in their own file so `tools.rs` and `helpers.rs`
//! stay under the 1000-line cap as the MCP surface grows. Both enumerate the workspace's files (the
//! code map, the document tier, and the files git tracks that neither holds) and share the same
//! limit/cursor/token-budget pagination shape; `find_files` additionally scores each candidate with
//! `nucleo-matcher`, and with [`within_one_edit`] for a query that names nothing as typed, before
//! paginating.

use std::sync::Arc;
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

/// The git index's file list and the unindexed share of it, as last read. Reused while the index
/// checksum and the code map's generation are unchanged, so a `find_files` call, or each page of
/// one, does not walk every index entry again.
#[derive(Default)]
pub(crate) struct TrackedCache {
    tracked: crate::git::TrackedFiles,
    generation: u32,
    unindexed: Arc<Vec<RelPath>>,
}

/// Files git tracks that neither the code map nor the document tier holds (images, fonts, data the
/// scan does not parse, and files the scan was told to leave out), in the index's path order, with a
/// stamp of the git index they were read from. Empty outside a git repository, and when the
/// workspace is a directory inside one: git's paths are then not relative to the workspace root.
///
/// The stamp joins the cursor's snapshot ([`snapshot`]), so a page asked for after `git add` or
/// `git rm` changed the listing is refused as stale instead of skipping or repeating rows.
fn unindexed_tracked(state: &ServerState, cache: &super::MapCache, generation: u32) -> (Arc<Vec<RelPath>>, u32) {
    let Some(repo) = state.shared.repo.as_ref() else {
        return (Arc::default(), 0);
    };
    if !repo.origin_is_workdir() {
        return (Arc::default(), 0);
    }
    let mut held = state
        .shared
        .tracked
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fresh = super::index_route::blocking(|| repo.tracked_files_since(held.tracked.checksum.as_ref()));
    match fresh {
        Ok(Some(tracked)) => held.tracked = tracked,
        Ok(None) if held.generation == generation => return (Arc::clone(&held.unindexed), stamp(&held.tracked)),
        Ok(None) => {}
        Err(_) => return (Arc::default(), 0),
    }
    let unindexed: Vec<RelPath> = held
        .tracked
        .files
        .iter()
        .filter(|path| !cache.contains(path) && !cache.contains_doc(path))
        .cloned()
        .collect();
    held.generation = generation;
    held.unindexed = Arc::new(unindexed);
    (Arc::clone(&held.unindexed), stamp(&held.tracked))
}

/// The first four bytes of the git index checksum; `0` when the index has none.
fn stamp(tracked: &crate::git::TrackedFiles) -> u32 {
    tracked.checksum.as_ref().map_or(0, |checksum| {
        let bytes = checksum.as_bytes();
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    })
}

/// A listing cursor's snapshot: the code map's generation with the git index's stamp folded in.
fn snapshot(generation: u32, stamp: u32) -> u32 {
    generation ^ stamp
}

/// Body of `find_files` without a query: enumerate the workspace's files with optional substring
/// (`path_contains`), prefix and `language` filters, then paginate. A file git tracks that is not
/// indexed has no language, so a `language` filter leaves it out.
pub(super) async fn run_list_files(state: &ServerState, params: ListFilesParams) -> Result<CallToolResult, McpError> {
    state.await_cache_ready().await;
    let (limit, limit_clamped) = super::tools::effective_list_limit(params.limit);
    let lang_filter = params.language.as_deref();
    let cache = state.shared.cache.load_full();
    let generation = state.shared.cache_generation.load(Ordering::Relaxed);
    let (tracked, stamp) = if lang_filter.is_none() {
        unindexed_tracked(state, &cache, generation)
    } else {
        (Arc::default(), 0)
    };
    let generation = snapshot(generation, stamp);

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
    let store = state.store_read().await;

    let path_finder = params
        .path_contains
        .as_ref()
        .map(|n| memchr::memmem::Finder::new(n.as_bytes()));
    let prefix_filter = params.path_prefix.as_deref().map(str::as_bytes);

    // Code map first, then the document tier (markdown, config, data, PDFs ...), then the tracked
    // files neither holds, each in sorted order. The index map iterates in hash order, which
    // differs per process; a cursor is an offset into this listing, so the order must be stable.
    let mut code_sorted: Vec<_> = store.index.files.iter().collect();
    code_sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let code = code_sorted.into_iter().map(|(p, e)| (p, Some(e.language.clone())));
    let doc_paths = std::sync::Arc::clone(&cache.doc_paths);
    let docs = doc_paths.iter().map(|p| (p, Some(language_of_doc(p))));
    let others = tracked.iter().map(|p| (p, None));

    let mut files: Vec<RelPath> = Vec::with_capacity(limit.min(256));
    let mut total: usize = 0;
    let mut seen: usize = 0;
    for (p, language) in code.chain(docs).chain(others) {
        let path_ok = path_finder.as_ref().is_none_or(|f| f.find(p.as_bytes()).is_some())
            && prefix_filter.is_none_or(|prefix| p.as_bytes().starts_with(prefix));
        let lang_ok = lang_filter.is_none_or(|l| language.as_deref() == Some(l));
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

/// Body of `find_files` with a query: fuzzy subsequence match over the workspace's files
/// (fzf/fd-style), ranked by `nucleo-matcher` score, with optional `path_prefix` / `language`
/// pre-filters. A query no path contains as typed may be a typo: files it names up to one edit
/// ([`within_one_edit`]) rank first.
///
/// Sourced from the `MapCache` file view (already sorted, in-RAM, no store lock needed) — the
/// `await_cache_ready()` call above guarantees it is populated before this runs. Paths that
/// aren't valid UTF-8 are skipped — `nucleo-matcher` scores `str`, not raw bytes.
pub(super) async fn run_find_files(state: &ServerState, params: FindFilesParams) -> Result<CallToolResult, McpError> {
    state.await_cache_ready().await;
    let (limit, limit_clamped) = super::tools::effective_list_limit(params.limit);
    let prefix_filter = params.path_prefix.as_deref();
    let lang_filter = params.language.as_deref();
    let cache = state.shared.cache.load_full();
    let generation = state.shared.cache_generation.load(Ordering::Relaxed);
    let (tracked, stamp) = if lang_filter.is_none() {
        unindexed_tracked(state, &cache, generation)
    } else {
        (Arc::default(), 0)
    };
    let generation = snapshot(generation, stamp);

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

    // Language and path are all the fuzzy-file search needs, and both live in the symbol-free file
    // view — so this whole scan reads no outline and touches no blob. The document tier (markdown,
    // JSON, YAML, PDFs ...) is not code-mapped, and the tracked files neither holds have no
    // language, but a file finder must cover them too.
    let code = cache.file_metas().map(|(p, meta)| (p, Some(meta.language.to_string())));
    let docs = cache.doc_paths.iter().map(|p| (p, Some(language_of_doc(p))));
    let others = tracked.iter().map(|p| (p, None));
    let candidates: Vec<&RelPath> = code
        .chain(docs)
        .chain(others)
        .filter(|(p, language)| {
            lang_filter.is_none_or(|l| language.as_deref() == Some(l))
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
/// only paths that happen to hold the letters in order. A query nucleo reads as more than one plain
/// atom (whitespace, or a `!`, `^`, `$` or `'` operator) names no one file, so it gets no typo pass.
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
    let operators = |c: char| c.is_whitespace() || matches!(c, '!' | '^' | '$' | '\'');
    if literal || needle.len() < TYPO_MIN_CHARS || needle.contains(operators) {
        return scored;
    }
    let typo_score = scored
        .iter()
        .map(|(score, _)| *score)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let mut at: ahash::AHashMap<&RelPath, usize> = scored.iter().enumerate().map(|(i, (_, path))| (*path, i)).collect();
    for &path in candidates {
        if names_with_typo(needle.as_bytes(), path.as_bytes()) {
            match at.get(path) {
                Some(&i) => scored[i].0 = typo_score,
                None => {
                    at.insert(path, scored.len());
                    scored.push((typo_score, path));
                }
            }
        }
    }
    scored
}

/// Shortest query a typo is looked for in: a shorter one is one edit away from too many names.
const TYPO_MIN_CHARS: usize = 4;

/// Whether `query` (ASCII-lowercased) names the file at `path` up to one edit: against the file
/// name, the file name without its extension, or, for a query holding `/`, the path's last segments.
/// Compared byte by byte, ASCII case aside, and without allocating: it runs on every candidate.
fn names_with_typo(query: &[u8], path: &[u8]) -> bool {
    let segments = memchr::memchr_iter(b'/', query).count() + 1;
    let tail_start = memchr::memrchr_iter(b'/', path)
        .nth(segments - 1)
        .map_or(0, |slash| slash + 1);
    let name_start = memchr::memrchr(b'/', path).map_or(0, |slash| slash + 1);
    let name = &path[name_start..];
    let stem = memchr::memrchr(b'.', name).map_or(name, |dot| &name[..dot]);
    [&path[tail_start..], name, stem]
        .iter()
        .any(|target| within_one_edit(query, target))
}

/// Whether `a` turns into `b` with at most one edit, ASCII case aside: two adjacent bytes swapped,
/// or one byte inserted, dropped or replaced. A length difference over one is refused first.
fn within_one_edit(a: &[u8], b: &[u8]) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if long.len() - short.len() > 1 {
        return false;
    }
    let same = |x: &[u8], y: &[u8]| x.eq_ignore_ascii_case(y);
    let Some(i) = short.iter().zip(long).position(|(x, y)| !x.eq_ignore_ascii_case(y)) else {
        return true;
    };
    if short.len() == long.len() {
        let swapped = i + 1 < short.len()
            && short[i].eq_ignore_ascii_case(&long[i + 1])
            && short[i + 1].eq_ignore_ascii_case(&long[i]);
        let rest = if swapped { i + 2 } else { i + 1 };
        same(&short[rest..], &long[rest..])
    } else {
        same(&short[i..], &long[i + 1..])
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
    use crate::engine::Lifecycle;
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

    fn chars(text: &str) -> &[u8] {
        text.as_bytes()
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
            assert!(within_one_edit(chars(typed), chars(meant)), "{typed} -> {meant}");
        }
        for (typed, meant) in [("bdiul", "build"), ("bnr", "banner"), ("tset", "testing")] {
            assert!(!within_one_edit(chars(typed), chars(meant)), "{typed} -> {meant}");
        }
    }

    #[test]
    fn a_typo_names_a_file_by_its_name_its_stem_or_its_last_segments() {
        assert!(names_with_typo(
            chars("blocking_cehck"),
            "tokio/src/util/blocking_check.rs".as_bytes()
        ));
        assert!(names_with_typo(chars("sceurity"), "SECURITY.md".as_bytes()));
        assert!(names_with_typo(
            chars("favcion-32"),
            "website/public/favicon-32.png".as_bytes()
        ));
        assert!(names_with_typo(
            chars("util/blocking_chekc.rs"),
            "tokio/src/util/blocking_check.rs".as_bytes()
        ));
        assert!(!names_with_typo(
            chars("blocking_cehck"),
            "tokio/src/util/blocking.rs".as_bytes()
        ));
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
    fn a_query_with_nucleo_operators_gets_no_typo_pass() {
        let paths = ["src/test.rs", "src/lib.rs", "conf.rs"].map(RelPath::from);
        let candidates: Vec<&RelPath> = paths.iter().collect();
        let found = |query: &str| -> Vec<String> {
            score(query, &candidates)
                .iter()
                .map(|(_, path)| path.to_string())
                .collect()
        };
        assert!(
            !found("!test").contains(&"src/test.rs".to_string()),
            "a negated atom excludes the file it is one edit from: {:?}",
            found("!test")
        );
        assert!(
            !found("^test").contains(&"src/test.rs".to_string()),
            "{:?}",
            found("^test")
        );
        assert!(!found("conf$").contains(&"conf.rs".to_string()), "{:?}", found("conf$"));
    }

    #[test]
    fn doc_language_labels() {
        assert_eq!(doc_language("docs/README.md"), "markdown");
        assert_eq!(doc_language("a/b.pdf"), "pdf");
        assert_eq!(doc_language("LICENSEFILE"), "document");
    }
}
