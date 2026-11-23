//! Typo-tolerant search over the git-history index.
//!
//! A query term is looked up exactly first. Only when the exact matches do not fill the page is the
//! term dictionary (`gh_term_dict`, the keys of `gh_term_to_ords` without their posting lists)
//! scanned once per field for the terms near each query term: within one or two edits (a
//! transposition counts as one), starting with the last query term, and, in the author field, holding
//! it as a subsequence. A query term with no posting of its own may also be two indexed terms run
//! together (`bulkconfig` → `bulk` + `config`). A commit found only through such a neighbour is
//! flagged `fuzzy`, and the commits found exactly come first.
//!
//! The other filters of a [`CommitQuery`] (a path, a time range, an author beside the message
//! terms) narrow both sets alike. Posting lists carry no term counts, so the order within each set
//! is newest first, as for every other history list, not a relevance score.

use ahash::{AHashMap, AHashSet};

use super::fts::{self, FIELD_AUTHOR, FtsScope};
use super::{GitHistoryIndex, LocalDb, encoding, keys};
use crate::git::CommitInfo;
use crate::path::RelPath;

/// Shortest term (in bytes) searched with edits: a shorter one is within an edit of too many words.
const EDIT_MIN_LEN: usize = 4;
/// Length from which a term tolerates two edits instead of one.
const TWO_EDITS_MIN_LEN: usize = 8;
/// Shortest term the last query term is completed from, and the shortest a subsequence match uses.
const PREFIX_MIN_LEN: usize = 3;
/// Dictionary terms a single query term expands to: the nearest are kept, so a short prefix of a
/// huge vocabulary stays bounded.
const MAX_EXPANSIONS: usize = 64;
/// Shortest part of a split term: a shorter one is a word of its own too often.
const SPLIT_MIN_PART_LEN: usize = 3;

/// What a search of the history asks for. Every present part must hold.
#[derive(Debug, Clone, Copy)]
pub struct CommitQuery<'a> {
    /// Words the `scope` fields must all contain, empty for none.
    pub pattern: &'a str,
    pub scope: FtsScope,
    /// Words the author name or email must all contain, on top of `pattern`.
    pub author: Option<&'a str>,
    /// A file the commit touched, or a directory (with a trailing `/`) it touched a file under.
    pub path: Option<&'a RelPath>,
    /// Author time bounds, Unix seconds, both inclusive.
    pub since: Option<i64>,
    pub until: Option<i64>,
}

/// One commit of a search, and whether it matched only through a tolerance.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub commit: CommitInfo,
    pub fuzzy: bool,
}

/// Words that must all be found in the fields of one scope.
struct Clause {
    terms: Vec<String>,
    scope: FtsScope,
}

/// A query term and what it may be completed or approximated to.
struct Probe<'a> {
    term: &'a str,
    /// The last term of its clause: also matched as a prefix.
    last: bool,
}

/// The author and path conditions that a candidate commit is read for.
struct CommitFilter {
    since: i64,
    until: i64,
    directory: Option<Vec<u8>>,
}

impl CommitFilter {
    fn needs_commit(&self) -> bool {
        self.since > i64::MIN || self.until < i64::MAX || self.directory.is_some()
    }
}

impl GitHistoryIndex {
    /// The commits matching `query`, exact matches first and then the tolerant ones, each group
    /// newest first, after skipping `skip` and taking at most `take`. A query with no word to
    /// search (`pattern` and `author` both empty or without a letter or digit) matches nothing.
    pub fn search_commits_fuzzy(&self, query: &CommitQuery<'_>, skip: usize, take: usize) -> Vec<SearchHit> {
        self.local().search_commits_fuzzy(query, skip, take)
    }

    /// Every path any indexed commit touched.
    pub fn history_paths(&self) -> Vec<RelPath> {
        self.local().history_paths()
    }
}

/// Edit distance between `a` and `b` (insert, delete, replace, or swap two neighbours, each one
/// edit), or `None` when it is over `max`. Over bytes, so a non-ASCII letter counts as several.
pub fn edit_distance_within(a: &[u8], b: &[u8], max: usize) -> Option<usize> {
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let width = b.len() + 1;
    let mut two_back = vec![0usize; width];
    let mut previous: Vec<usize> = (0..width).collect();
    let mut current = vec![0usize; width];
    for (i, &x) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, &y) in b.iter().enumerate() {
            let replace = previous[j] + usize::from(x != y);
            let mut best = replace.min(previous[j + 1] + 1).min(current[j] + 1);
            if i > 0 && j > 0 && x == b[j - 1] && a[i - 1] == y {
                best = best.min(two_back[j - 1] + 1);
            }
            current[j + 1] = best;
        }
        if current.iter().min().is_some_and(|&row| row > max) {
            return None;
        }
        std::mem::swap(&mut two_back, &mut previous);
        std::mem::swap(&mut previous, &mut current);
    }
    Some(previous[b.len()]).filter(|&distance| distance <= max)
}

/// Edits a term of `len` bytes tolerates.
fn edits_allowed(len: usize) -> usize {
    match len {
        0..EDIT_MIN_LEN => 0,
        EDIT_MIN_LEN..TWO_EDITS_MIN_LEN => 1,
        _ => 2,
    }
}

/// Whether the bytes of `needle` occur in `haystack` in order, not necessarily next to each other.
fn is_subsequence(needle: &[u8], haystack: &[u8]) -> bool {
    let mut rest = haystack.iter();
    needle.iter().all(|byte| rest.any(|candidate| candidate == byte))
}

/// How far `dictionary_term` is from `probe`: 1 or 2 for a term within that many edits, 1 for one
/// the last term is a prefix of, 3 for one holding it as a subsequence (author field only), and
/// `None` for no match. Lower is nearer.
fn nearness(probe: &Probe<'_>, dictionary_term: &[u8], subsequence: bool) -> Option<usize> {
    let wanted = probe.term.as_bytes();
    if dictionary_term == wanted {
        return Some(0);
    }
    if probe.last && wanted.len() >= PREFIX_MIN_LEN && dictionary_term.starts_with(wanted) {
        return Some(1);
    }
    if let Some(distance) = edit_distance_within(wanted, dictionary_term, edits_allowed(wanted.len())) {
        return Some(distance);
    }
    (subsequence && wanted.len() >= PREFIX_MIN_LEN && is_subsequence(wanted, dictionary_term)).then_some(3)
}

fn clauses(query: &CommitQuery<'_>) -> Vec<Clause> {
    let mut clauses = Vec::new();
    if !query.pattern.trim().is_empty() {
        clauses.push(Clause {
            terms: fts::query_terms(query.pattern),
            scope: query.scope,
        });
    }
    if let Some(author) = query.author {
        clauses.push(Clause {
            terms: fts::query_terms(author),
            scope: FtsScope::Author,
        });
    }
    clauses
}

impl LocalDb {
    fn search_commits_fuzzy(&self, query: &CommitQuery<'_>, skip: usize, take: usize) -> Vec<SearchHit> {
        let clauses = clauses(query);
        if take == 0 || clauses.is_empty() || clauses.iter().any(|clause| clause.terms.is_empty()) {
            return Vec::new();
        }
        let filter = CommitFilter {
            since: query.since.unwrap_or(i64::MIN),
            until: query.until.unwrap_or(i64::MAX),
            directory: query
                .path
                .filter(|path| path.as_bytes().ends_with(b"/"))
                .map(|path| path.as_bytes().to_vec()),
        };
        let file_ords = match query.path.filter(|path| !path.as_bytes().ends_with(b"/")) {
            Some(file) => match self.file_ords(file) {
                Some(ords) => Some(ords),
                None => return Vec::new(),
            },
            None => None,
        };

        let mut exact = self.exact_ords(&clauses);
        if let Some(file_ords) = &file_ords {
            exact.retain(|ord| file_ords.contains(ord));
        }
        let mut page = Page::new(skip, take);
        let mut paths = AHashMap::new();
        let mut exact_sorted: Vec<u32> = exact.iter().copied().collect();
        exact_sorted.sort_unstable_by(|a, b| b.cmp(a));
        for ord in exact_sorted {
            if self.passes(ord, &filter, &mut paths) && page.add(ord, false) {
                return self.hits(page.taken);
            }
        }

        let mut tolerant = self.tolerant_ords(&clauses);
        tolerant.retain(|ord| !exact.contains(ord) && file_ords.as_ref().is_none_or(|ords| ords.contains(ord)));
        let mut tolerant_sorted: Vec<u32> = tolerant.into_iter().collect();
        tolerant_sorted.sort_unstable_by(|a, b| b.cmp(a));
        for ord in tolerant_sorted {
            if self.passes(ord, &filter, &mut paths) && page.add(ord, true) {
                break;
            }
        }
        self.hits(page.taken)
    }

    /// Whether the commit `ord` is inside the time range and under the directory, read only when
    /// the filter asks.
    fn passes(&self, ord: u32, filter: &CommitFilter, under: &mut AHashMap<u32, bool>) -> bool {
        if !filter.needs_commit() {
            return true;
        }
        let Some(meta) = self.commit_meta(ord, filter.directory.is_some()) else {
            return false;
        };
        if meta.author_time_unix < filter.since || meta.author_time_unix > filter.until {
            return false;
        }
        let Some(directory) = &filter.directory else {
            return true;
        };
        meta.files.iter().any(|&(path_id, _)| {
            *under.entry(path_id).or_insert_with(|| {
                self.path_for_id(path_id)
                    .is_some_and(|path| path.as_bytes().starts_with(directory))
            })
        })
    }

    fn hits(&self, taken: Vec<(u32, bool)>) -> Vec<SearchHit> {
        let mut cache = AHashMap::new();
        taken
            .into_iter()
            .filter_map(|(ord, fuzzy)| {
                let meta = self.commit_meta(ord, false)?;
                let mut commit = self.meta_to_info(meta, &mut cache);
                commit.body = self.commit_text(ord).unwrap_or_default();
                Some(SearchHit { commit, fuzzy })
            })
            .collect()
    }

    /// The commits holding every word of every clause as typed.
    fn exact_ords(&self, clauses: &[Clause]) -> AHashSet<u32> {
        let mut matching: Option<AHashSet<u32>> = None;
        for clause in clauses {
            for term in &clause.terms {
                let ords = self.ords_for_term(term, clause.scope);
                matching = Some(intersect(matching, ords));
                if matching.as_ref().is_some_and(|set| set.is_empty()) {
                    return AHashSet::new();
                }
            }
        }
        matching.unwrap_or_default()
    }

    /// The commits holding, for every word of every clause, the word or a term near it.
    fn tolerant_ords(&self, clauses: &[Clause]) -> AHashSet<u32> {
        let mut matching: Option<AHashSet<u32>> = None;
        for clause in clauses {
            let probes: Vec<Probe<'_>> = clause
                .terms
                .iter()
                .enumerate()
                .map(|(at, term)| Probe {
                    term,
                    last: at + 1 == clause.terms.len(),
                })
                .collect();
            let near: Vec<(u8, Vec<Vec<Vec<u8>>>)> = clause
                .scope
                .fields()
                .iter()
                .map(|&field| (field, self.expand(field, &probes)))
                .collect();
            for (at, probe) in probes.iter().enumerate() {
                let mut ords = self.ords_for_term(probe.term, clause.scope);
                let held_as_typed = !ords.is_empty();
                for (field, expansions) in &near {
                    for term in &expansions[at] {
                        if let Some(bytes) = self.term_posting_bytes(&keys::term_key(*field, term)) {
                            ords.extend(encoding::decode_ords(&bytes));
                        }
                    }
                }
                if !held_as_typed {
                    ords.extend(self.joined_terms(probe.term, clause.scope));
                }
                matching = Some(intersect(matching, ords));
                if matching.as_ref().is_some_and(|set| set.is_empty()) {
                    return AHashSet::new();
                }
            }
        }
        matching.unwrap_or_default()
    }

    /// For each probe, the dictionary terms of `field` near it, nearest first and at most
    /// [`MAX_EXPANSIONS`]: one pass over the field's vocabulary for all probes.
    fn expand(&self, field: u8, probes: &[Probe<'_>]) -> Vec<Vec<Vec<u8>>> {
        let subsequence = field == FIELD_AUTHOR;
        let mut found: Vec<Vec<(usize, Vec<u8>)>> = vec![Vec::new(); probes.len()];
        for guard in self.term_dict.prefix([field]) {
            let Ok((key, _)) = guard.into_inner() else {
                continue;
            };
            let term = &key[1..];
            for (probe, near) in probes.iter().zip(found.iter_mut()) {
                if let Some(cost) = nearness(probe, term, subsequence) {
                    near.push((cost, term.to_vec()));
                }
            }
        }
        found
            .into_iter()
            .map(|mut near| {
                near.sort_by(|a, b| (a.0, a.1.len(), &a.1).cmp(&(b.0, b.1.len(), &b.1)));
                near.truncate(MAX_EXPANSIONS);
                near.into_iter().map(|(_, term)| term).collect()
            })
            .collect()
    }

    /// The commits holding `term` as two indexed words run together (`bulkconfig` for `bulk_config`),
    /// split anywhere both halves are at least [`SPLIT_MIN_PART_LEN`] long.
    fn joined_terms(&self, term: &str, scope: FtsScope) -> AHashSet<u32> {
        let mut out = AHashSet::new();
        for at in SPLIT_MIN_PART_LEN..=term.len().saturating_sub(SPLIT_MIN_PART_LEN) {
            let Some((head, tail)) = term.split_at_checked(at) else {
                continue;
            };
            let head = self.ords_for_term(head, scope);
            if head.is_empty() {
                continue;
            }
            let tail = self.ords_for_term(tail, scope);
            out.extend(head.into_iter().filter(|ord| tail.contains(ord)));
        }
        out
    }

    /// The commits that touched `file`, or `None` when no indexed commit did.
    fn file_ords(&self, file: &RelPath) -> Option<AHashSet<u32>> {
        let path_id = self.path_id(file)?;
        let bytes = self.posting_bytes(path_id)?;
        Some(encoding::decode_ords(&bytes).into_iter().collect())
    }

    fn history_paths(&self) -> Vec<RelPath> {
        self.path_id_by_path
            .iter()
            .filter_map(|guard| {
                let (key, _) = guard.into_inner().ok()?;
                key.get(2..).map(RelPath::from)
            })
            .collect()
    }
}

fn intersect(matching: Option<AHashSet<u32>>, ords: AHashSet<u32>) -> AHashSet<u32> {
    match matching {
        None => ords,
        Some(mut acc) => {
            acc.retain(|ord| ords.contains(ord));
            acc
        }
    }
}

/// The window `[skip, skip + take)` of an ordered stream of hits.
struct Page {
    skip: usize,
    take: usize,
    seen: usize,
    taken: Vec<(u32, bool)>,
}

impl Page {
    fn new(skip: usize, take: usize) -> Self {
        Self {
            skip,
            take,
            seen: 0,
            taken: Vec::new(),
        }
    }

    /// Offer the next hit; true once the window is full.
    fn add(&mut self, ord: u32, fuzzy: bool) -> bool {
        self.seen += 1;
        if self.seen > self.skip {
            self.taken.push((ord, fuzzy));
        }
        self.taken.len() >= self.take
    }
}

#[cfg(test)]
#[path = "fuzzy_tests.rs"]
mod tests;
