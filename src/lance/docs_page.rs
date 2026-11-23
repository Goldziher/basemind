//! A page of a document-search ranking (ADR-0029 rule 8): merge the chunks of one document that
//! touch or overlap into one hit (at the best rank of its chunks), let no document fill the page
//! (`per_doc`), cut the page, add a neighbouring chunk on either side when asked and the token
//! budget allows, and cite every hit.

use ahash::{AHashMap, AHashSet};

use super::docs::DocStore;
use super::docs_schema::{DocChunkHit, DocMeta};
use super::docs_types::{DocChunkRef, DocCite, DocContext, DocHit, DocsPage, DocsRanking, RankedChunk};

/// Hits of a ranking that can be paged through.
pub const RANKED_DEPTH: usize = super::docs_rank::RANKED_FETCH - 1;
/// The longest overlap between consecutive chunks that stitching looks for, in bytes.
const STITCH_MAX_OVERLAP: usize = 512;
/// The shortest shared text taken for an overlap: a shorter match is a coincidence ("the" + "end").
const STITCH_MIN_OVERLAP: usize = 12;

/// Hits one document contributes before the other documents' hits rank ahead of its rest.
pub const PER_DOC_DEFAULT: u32 = 2;

/// Which part of a ranking one page is, and how it is shaped.
#[derive(Debug, Clone, Copy)]
pub struct Paging {
    /// Hits (merged runs of chunks) of the ranking before the page.
    pub offset: u32,
    pub limit: u32,
    /// Token budget of the page; the first hit is always kept.
    pub max_tokens: Option<u32>,
    /// Most hits of one document ahead of the rest of the ranking; 0 is no cap.
    pub per_doc: u32,
    /// Add the chunk before and after each hit.
    pub context: bool,
}

/// Estimated tokens of `bytes` of response, the estimate the MCP token budget uses.
fn bytes_to_tokens(bytes: u64) -> u64 {
    bytes / 4
}

/// One hit of a page before its context is added: a run of adjacent chunks of one document.
struct Merged {
    first: RankedChunk,
    members: Vec<RankedChunk>,
}

impl Merged {
    fn ord_range(&self) -> (u32, u32) {
        let ords = self.members.iter().map(|m| m.ord);
        (ords.clone().min().unwrap_or(0), ords.max().unwrap_or(0))
    }
}

/// Group the ranking's hits: a hit whose chunk touches or overlaps the chunks of an earlier hit of
/// the same document joins that hit, wherever the two rank. A group keeps the rank of its best
/// chunk, so it ranks where its first member did.
fn merge(ranking: Vec<RankedChunk>) -> Vec<Merged> {
    let mut groups: Vec<Merged> = Vec::new();
    for hit in ranking {
        let touches = |group: &Merged| {
            group.first.doc == hit.doc && {
                let (lo, hi) = group.ord_range();
                hit.ord + 1 >= lo && hit.ord <= hi + 1
            }
        };
        let Some(first) = groups.iter().position(touches) else {
            groups.push(Merged {
                first: hit.clone(),
                members: vec![hit],
            });
            continue;
        };
        // A chunk can bridge groups that did not touch before: they all join the best-ranked one.
        // Groups do not touch one another, so the ones the chunk reaches are the only ones.
        let bridged: Vec<usize> = (first + 1..groups.len()).filter(|&at| touches(&groups[at])).collect();
        for at in bridged.into_iter().rev() {
            let other = groups.remove(at);
            groups[first].members.extend(other.members);
        }
        groups[first].members.push(hit);
    }
    for group in &mut groups {
        group.members.sort_by_key(|member| member.ord);
        group.members.dedup_by_key(|member| member.ord);
    }
    groups
}

/// `groups` with no document holding more than `per_doc` places ahead of the others: the hits past
/// the cap follow the rest in their own order, so a document that dominates the ranking does not
/// hide the others and nothing is dropped. `0` leaves the order as it is.
fn diversify(groups: Vec<Merged>, per_doc: u32) -> Vec<Merged> {
    if per_doc == 0 {
        return groups;
    }
    let mut seen: AHashMap<String, u32> = AHashMap::new();
    let (mut head, mut tail) = (Vec::new(), Vec::new());
    for group in groups {
        let count = seen.entry(group.first.doc.clone()).or_default();
        *count += 1;
        if *count <= per_doc {
            head.push(group);
        } else {
            tail.push(group);
        }
    }
    head.append(&mut tail);
    head
}

/// `b` appended to `a`, without the text the two chunks share at the seam (xberg overlaps
/// consecutive chunks).
fn stitch(a: &str, b: &str) -> String {
    let max = a.len().min(b.len()).min(STITCH_MAX_OVERLAP);
    let overlap = (STITCH_MIN_OVERLAP..=max)
        .rev()
        .find(|&n| a.is_char_boundary(a.len() - n) && b.is_char_boundary(n) && a[a.len() - n..] == b[..n])
        .unwrap_or(0);
    let rest = &b[overlap..];
    if overlap == 0 && !a.ends_with('\n') && !rest.starts_with('\n') {
        format!("{a}\n{rest}")
    } else {
        format!("{a}{rest}")
    }
}

fn union(cites: impl Iterator<Item = DocCite>) -> DocCite {
    let mut out: Option<DocCite> = None;
    for cite in cites {
        out = Some(match out {
            None => cite,
            Some(mut acc) => {
                acc.page_start = min_opt(acc.page_start, cite.page_start);
                acc.page_end = acc.page_end.max(cite.page_end);
                acc.line_start = min_opt(acc.line_start, cite.line_start);
                acc.line_end = acc.line_end.max(cite.line_end);
                acc.byte_start = acc.byte_start.min(cite.byte_start);
                acc.byte_end = acc.byte_end.max(cite.byte_end);
                acc
            }
        });
    }
    out.unwrap_or_default()
}

fn min_opt(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The Markdown headings (ATX, `#` to `######`) that start a line of `text`, without their marks.
/// Fenced code is skipped: a `#` comment in a code block is not a heading.
fn headings_in(text: &str) -> Vec<String> {
    let mut fenced = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced || line.len() - trimmed.len() > 3 {
            continue;
        }
        let level = trimmed.bytes().take_while(|&b| b == b'#').count();
        let rest = &trimmed[level..];
        if (1..=6).contains(&level) && (rest.is_empty() || rest.starts_with(' ')) {
            let heading = rest.trim().trim_end_matches('#').trim();
            if !heading.is_empty() {
                out.push(heading.to_string());
            }
        }
    }
    out
}

fn tokens_of<T: serde::Serialize>(value: &T) -> u64 {
    serde_json::to_vec(value).map_or(0, |bytes| bytes_to_tokens(bytes.len() as u64))
}

fn hit_of(group: &Merged, title: Option<String>, of: Option<u32>) -> DocHit {
    let (ord, ord_end) = group.ord_range();
    let text = group
        .members
        .iter()
        .skip(1)
        .fold(group.members[0].text.clone(), |acc, member| stitch(&acc, &member.text));
    let mut cite = union(group.members.iter().map(|member| member.cite.clone()));
    cite.section = group.members[0].cite.section.clone();
    cite.headings = headings_in(&text);
    DocHit {
        path: group.first.paths.first().cloned().unwrap_or_default(),
        also_at: group.first.paths.iter().skip(1).cloned().collect(),
        mime_type: group.first.mime_type.clone(),
        title,
        package: None,
        package_version: None,
        chunk: DocChunkRef {
            doc: group.first.doc.clone(),
            ord,
            ord_end,
            of,
        },
        cite,
        text,
        context: DocContext::default(),
        score: group.first.score,
        lanes: group.first.lanes,
        rerank_score: group.first.rerank_score,
    }
}

/// Cut the page of `ranking` that `paging` names and build its hits, reading titles (and, with
/// `paging.context`, neighbouring chunks) from `store`. The first hit is always kept.
pub fn page(store: Option<&DocStore>, ranking: DocsRanking, paging: Paging) -> DocsPage {
    let Paging {
        offset,
        limit,
        max_tokens,
        per_doc,
        context,
    } = paging;
    let DocsRanking {
        query,
        mut hits,
        lanes_ran,
        lanes_unavailable,
        lexical_fuzzy,
        vector_mode,
        rerank,
        notice,
        truncated_reason,
    } = ranking;
    let goes_on = hits.len() > RANKED_DEPTH;
    hits.truncate(RANKED_DEPTH);
    let all = diversify(merge(hits), per_doc);
    let fetched = all.len() as u32;
    let groups: Vec<Merged> = all.into_iter().skip(offset as usize).take(limit as usize).collect();
    let consumed = groups.len() as u32;

    let (metas, neighbours) = match store {
        Some(store) if !groups.is_empty() => context_of(store, &groups, context),
        _ => (AHashMap::new(), AHashMap::new()),
    };

    let mut out: Vec<DocHit> = groups
        .iter()
        .map(|group| {
            let meta = metas.get(&group.first.doc);
            let mut hit = hit_of(group, meta.and_then(|m| m.title.clone()), meta.map(|m| m.n_chunks));
            // A content shared with a repository file cites a package only from a dependency path.
            if let Some(meta) = meta
                && group.first.paths.iter().any(|path| crate::deps_docs::is_virtual(path))
            {
                hit.package.clone_from(&meta.package);
                hit.package_version.clone_from(&meta.package_version);
            }
            hit
        })
        .collect();
    let budget = max_tokens.map(u64::from);
    let mut used: u64 = 0;
    let mut kept = 0;
    for hit in &out {
        let cost = tokens_of(hit);
        if kept > 0 && budget.is_some_and(|budget| used + cost > budget) {
            break;
        }
        used += cost;
        kept += 1;
    }
    let budgeted = kept < out.len();
    out.truncate(kept);
    // Context goes on in rank order while the budget holds: before, then after. A chunk already
    // given to an earlier hit is not repeated.
    let mut given: AHashSet<(String, u32)> = AHashSet::new();
    for (hit, group) in out.iter_mut().zip(&groups) {
        let doc = &group.first.doc;
        let (ord, ord_end) = group.ord_range();
        let before = ord.checked_sub(1).map(|o| (doc.clone(), o));
        let after = Some((doc.clone(), ord_end + 1));
        for (key, is_before) in [(before, true), (after, false)] {
            let Some(key) = key else { continue };
            let Some(chunk) = neighbours.get(&key).filter(|_| !given.contains(&key)) else {
                continue;
            };
            let cost = bytes_to_tokens(chunk.text.len() as u64);
            if budget.is_some_and(|budget| used + cost > budget) {
                continue;
            }
            used += cost;
            given.insert(key);
            if is_before {
                hit.text = stitch(&chunk.text, &hit.text);
                hit.context.before = 1;
            } else {
                hit.text = stitch(&hit.text, &chunk.text);
                hit.context.after = 1;
            }
        }
    }

    // The page ends after its last kept hit.
    let next = offset + if budgeted { kept as u32 } else { consumed };
    let more = !out.is_empty() && next < fetched;
    // Only a ranking the page has read to its end is cut at the depth: a `limit` cut is paging.
    let truncated_reason = truncated_reason.or_else(|| (goes_on && !more).then(|| "depth".to_string()));
    DocsPage {
        query,
        hits: out,
        lanes_ran,
        lanes_unavailable,
        lexical_fuzzy,
        vector_mode,
        rerank,
        notice,
        truncated: truncated_reason.is_some(),
        truncated_reason,
        budgeted,
        next_offset: more.then_some(next),
    }
}

type Metas = AHashMap<String, DocMeta>;
type Neighbours = AHashMap<(String, u32), DocChunkHit>;

/// Titles and chunk counts of the page's documents, and with `context` the chunks on either side of
/// each group. A store error leaves the page without context rather than failing it.
fn context_of(store: &DocStore, groups: &[Merged], context: bool) -> (Metas, Neighbours) {
    let mut keys: Vec<String> = groups.iter().map(|g| g.first.doc.clone()).collect();
    keys.sort_unstable();
    keys.dedup();
    let mut wanted: AHashMap<String, Vec<u32>> = AHashMap::new();
    for group in groups.iter().filter(|_| context) {
        let (ord, ord_end) = group.ord_range();
        let ords = wanted.entry(group.first.doc.clone()).or_default();
        if let Some(before) = ord.checked_sub(1) {
            ords.push(before);
        }
        ords.push(ord_end + 1);
    }
    let wanted: Vec<(String, Vec<u32>)> = wanted.into_iter().collect();
    let fetched = store.documents(&keys).and_then(|metas| {
        let chunks = if wanted.is_empty() {
            Vec::new()
        } else {
            store.chunks(&wanted)?
        };
        Ok((metas, chunks))
    });
    match fetched {
        Ok((metas, chunks)) => (
            metas.into_iter().map(|meta| (meta.doc_key.clone(), meta)).collect(),
            chunks
                .into_iter()
                .map(|chunk| ((chunk.doc_key.clone(), chunk.chunk_ord), chunk))
                .collect(),
        ),
        Err(error) => {
            tracing::warn!(%error, "docs_search: reading context chunks failed; the page carries none");
            (AHashMap::new(), AHashMap::new())
        }
    }
}

#[cfg(test)]
#[path = "docs_page_tests.rs"]
mod tests;
