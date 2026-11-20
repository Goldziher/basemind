//! How each `code_refs` call site was resolved (ADR-0047 decision 6), the page's most common value,
//! and the `resolution` filter.
//!
//! Computed at query time from facts already indexed: the `refs_by_path` binding of a use, the
//! outline of its file, and `symbols_by_name`. Nothing is stored per row, so a value cannot go stale
//! with a cross-file change.

use super::calls_scan::{BudgetedCallPage, CallScanPage, budget_call_page};
use crate::engine::MapCache;
use crate::engine::files::IndexedFiles;
use crate::engine::types::refs::{Resolution, ResolutionCounts, ResolutionFilter};
use crate::index::view::IndexView;
use crate::path::RelPath;

/// Distinct callees one page probes `symbols_by_name` for. A `substring` page can hold many; past
/// the cap a site with no binding stays `name_only`, since nothing showed the name is undefined.
const EXTERNAL_PROBES_MAX: usize = 256;

/// The indexed bytes of the files a page checks bindings in, each read once.
pub(crate) struct UseSources<'a> {
    files: IndexedFiles<'a>,
    cache: &'a MapCache,
    read: ahash::AHashMap<RelPath, Option<Vec<u8>>>,
}

impl<'a> UseSources<'a> {
    pub(crate) fn new(files: IndexedFiles<'a>, cache: &'a MapCache) -> Self {
        Self {
            files,
            cache,
            read: ahash::AHashMap::new(),
        }
    }

    /// Whether the identifier at `start` of `path`, as indexed, is `callee`. A call site starts
    /// where its call expression does, which for a member call (`util.target()`) is the receiver,
    /// so a binding found there is the receiver's and says nothing of the callee. `false` when the
    /// file was edited since the scan or cannot be read: the offset then names nothing.
    fn names(&mut self, path: &RelPath, start: u32, callee: &str) -> bool {
        let (files, cache) = (self.files, self.cache);
        let bytes = self.read.entry(path.clone()).or_insert_with(|| {
            let meta = cache.meta(path)?;
            let file = files.indexed_path(path)?;
            let live = crate::engine::freshness::load(&file, meta.into(), files.max_file_bytes).ok()?;
            live.fresh.then_some(live.bytes)
        });
        bytes
            .as_deref()
            .is_some_and(|bytes| super::intel::identifier_at(bytes, start) == callee)
    }

    /// The definition the callee of the call site at `start` of `path` binds to, if resolution
    /// facts hold one.
    fn binding(&mut self, idx: &dyn IndexView, path: &RelPath, start: u32, callee: &str) -> Option<(RelPath, u32)> {
        idx.definition_of(path, start)
            .filter(|_| self.names(path, start, callee))
    }
}

/// Set the [`Resolution`] of each hit of a name scan (no pinned definition), parallel to
/// `starts`, the call sites' start bytes.
///
/// A site bound to another file is `imported`. A site bound in its own file is `same_file` when the
/// binding lands inside a symbol of the callee's name there, and `scope_resolved` otherwise (a local
/// variable or parameter, or an import binding whose target is not indexed). A site with no binding
/// is `external` when the index defines no symbol of the name, and otherwise `name_only`.
pub(crate) fn classify_references(
    idx: &dyn IndexView,
    sources: &mut UseSources<'_>,
    hits: &mut [crate::engine::types::refs::ReferenceHit],
    starts: &[u32],
) {
    let cache = sources.cache;
    let mut supported: ahash::AHashMap<RelPath, bool> = ahash::AHashMap::new();
    let mut defined: ahash::AHashMap<String, bool> = ahash::AHashMap::new();
    for (hit, start) in hits.iter_mut().zip(starts) {
        let has_facts = *supported.entry(hit.path.clone()).or_insert_with(|| {
            cache
                .language_of(&hit.path)
                .is_some_and(crate::intel::resolve::supports)
        });
        let binding = if has_facts {
            sources.binding(idx, &hit.path, *start, &hit.callee)
        } else {
            None
        };
        hit.resolution = match binding {
            Some((def_path, _)) if def_path != hit.path => Resolution::Imported,
            Some((_, def_start)) if defines_at(cache, &hit.path, &hit.callee, def_start) => Resolution::SameFile,
            Some(_) => Resolution::ScopeResolved,
            None => {
                let probes = defined.len();
                match defined.get(&hit.callee) {
                    Some(true) => Resolution::NameOnly,
                    Some(false) => Resolution::External,
                    None if probes >= EXTERNAL_PROBES_MAX => Resolution::NameOnly,
                    None => {
                        let is_defined = !idx.symbols_by_name_lookup(&hit.callee, 1).is_empty();
                        defined.insert(hit.callee.clone(), is_defined);
                        if is_defined {
                            Resolution::NameOnly
                        } else {
                            Resolution::External
                        }
                    }
                }
            }
        };
    }
}

/// Whether the outline of `path` holds a symbol named `name` whose span contains `def_start`.
fn defines_at(cache: &MapCache, path: &RelPath, name: &str, def_start: u32) -> bool {
    cache.get(path).is_some_and(|l1| {
        l1.symbols
            .iter()
            .any(|symbol| symbol.name == name && (symbol.start_byte..symbol.end_byte).contains(&def_start))
    })
}

/// Turn each `elsewhere` hit of a pinned-definition page whose callee binds to nothing into
/// `name_only`: resolution did not prove it binds to the definition, and has no binding for it
/// either, so it was never checked. `starts` is parallel to `hits`.
pub(crate) fn confirm_elsewhere(
    idx: &dyn IndexView,
    sources: &mut UseSources<'_>,
    hits: &mut [crate::engine::types::refs::ReferenceHit],
    starts: &[u32],
) {
    for (hit, start) in hits.iter_mut().zip(starts) {
        if hit.resolution == Resolution::Elsewhere && sources.binding(idx, &hit.path, *start, &hit.callee).is_none() {
            hit.resolution = Resolution::NameOnly;
        }
    }
}

/// A call-scan page as it is answered: filtered by `resolution`, budgeted, and with the page's
/// `resolution_default` and `resolution_counts`.
pub(crate) struct FinishedPage {
    pub(crate) page: BudgetedCallPage,
    pub(crate) default: Option<Resolution>,
    pub(crate) counts: Option<ResolutionCounts>,
}

/// Filter `page` by `filter`, budget it ([`budget_call_page`]), and elide each row's resolution that
/// is the page's most common one.
///
/// The counts cover the rows this page answers for before the filter: the whole page, or, when the
/// budget cut it, the rows up to the last one kept, since the next page resumes after that one.
/// They are left out when they say nothing the rows do not: every row is the default.
pub(crate) fn finish_page(
    mut page: CallScanPage,
    filter: Option<ResolutionFilter>,
    name: &str,
    max_tokens: Option<u32>,
) -> FinishedPage {
    let before: Vec<Resolution> = page.hits.iter().map(|hit| hit.resolution).collect();
    // Index into `before` of each row the filter keeps, in scan order.
    let mut kept_at: Vec<usize> = (0..before.len()).collect();
    if let Some(filter) = filter {
        let mask: Vec<bool> = before.iter().map(|resolution| filter.admits(*resolution)).collect();
        kept_at.retain(|at| mask[*at]);
        retain_by(&mut page.hits, &mask);
        retain_by(&mut page.hit_keys, &mask);
        retain_by(&mut page.hit_starts, &mask);
    }

    // The budget costs each row as written, so the elision is set before it.
    let provisional = most_common(page.hits.iter().map(|hit| hit.resolution));
    for hit in &mut page.hits {
        hit.elide_resolution = Some(hit.resolution) == provisional;
    }
    let mut budgeted = budget_call_page(page, name, max_tokens);
    let default = most_common(budgeted.hits.iter().map(|hit| hit.resolution));
    for hit in &mut budgeted.hits {
        hit.elide_resolution = Some(hit.resolution) == default;
    }

    let answered = if budgeted.budgeted {
        budgeted
            .hits
            .len()
            .checked_sub(1)
            .and_then(|last| kept_at.get(last))
            .map_or(0, |at| at + 1)
    } else {
        before.len()
    };
    let mut counts = ResolutionCounts::new();
    for resolution in &before[..answered] {
        *counts.entry(*resolution).or_default() += 1;
    }
    let says_nothing_more = counts.is_empty() || (counts.len() == 1 && counts.keys().next() == default.as_ref());
    FinishedPage {
        page: budgeted,
        default,
        counts: (!says_nothing_more).then_some(counts),
    }
}

/// Keep the items of `items` whose `mask` entry is set.
fn retain_by<T>(items: &mut Vec<T>, mask: &[bool]) {
    let mut flags = mask.iter();
    items.retain(|_| flags.next().copied().unwrap_or(false));
}

/// The most common value, the earliest in [`Resolution`] order on a tie; `None` for no values.
fn most_common(values: impl Iterator<Item = Resolution>) -> Option<Resolution> {
    let mut counts = [0u32; Resolution::ALL.len()];
    for value in values {
        counts[value as usize] += 1;
    }
    let mut best: Option<(Resolution, u32)> = None;
    for (resolution, count) in Resolution::ALL.into_iter().zip(counts) {
        if count > 0 && best.is_none_or(|(_, top)| count > top) {
            best = Some((resolution, count));
        }
    }
    best.map(|(resolution, _)| resolution)
}

#[cfg(test)]
#[path = "resolution_tests.rs"]
mod tests;
