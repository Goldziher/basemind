//! A page of a document-search ranking (ADR-0029 rule 8): cut the page, merge adjacent chunks of
//! one document that are rank neighbours, add a neighbouring chunk on either side while the token
//! budget allows, and cite every hit.

use ahash::AHashMap;

use super::docs::DocStore;
use super::docs_schema::DocChunkHit;
use super::docs_types::{DocChunkRef, DocCite, DocContext, DocHit, DocsPage, DocsRanking, RankedChunk};

/// Hits of a ranking that can be paged through.
pub const RANKED_DEPTH: usize = super::docs_rank::RANKED_FETCH - 1;
/// The longest overlap between consecutive chunks that stitching looks for, in bytes.
const STITCH_MAX_OVERLAP: usize = 512;
/// The shortest shared text taken for an overlap: a shorter match is a coincidence ("the" + "end").
const STITCH_MIN_OVERLAP: usize = 12;

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

/// Group the page's hits: a hit whose chunk is next to a chunk of the hit ranked just before it, in
/// the same document, joins that hit. Only rank neighbours merge, so every group is a run of
/// consecutive ranks and the page can end after any group. The group keeps its first rank.
fn merge(page: Vec<RankedChunk>) -> Vec<Merged> {
    let mut groups: Vec<Merged> = Vec::new();
    for hit in page {
        let joined = groups.last_mut().filter(|group| {
            group.first.doc == hit.doc && {
                let (lo, hi) = group.ord_range();
                hit.ord + 1 >= lo && hit.ord <= hi + 1
            }
        });
        match joined {
            Some(group) => group.members.push(hit),
            None => groups.push(Merged {
                first: hit.clone(),
                members: vec![hit],
            }),
        }
    }
    for group in &mut groups {
        group.members.sort_by_key(|member| member.ord);
    }
    groups
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

/// Cut the page `[offset, offset + limit)` out of `ranking` and build its hits, reading titles and
/// neighbouring chunks from `store`. `max_tokens` bounds the hits; the first is always kept.
pub fn page(
    store: Option<&DocStore>,
    ranking: DocsRanking,
    offset: u32,
    limit: u32,
    max_tokens: Option<u32>,
) -> DocsPage {
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
    let fetched = hits.len() as u32;
    let page: Vec<RankedChunk> = hits.into_iter().skip(offset as usize).take(limit as usize).collect();
    let consumed = page.len() as u32;
    let groups = merge(page);

    let (metas, neighbours) = match store {
        Some(store) if !groups.is_empty() => context_of(store, &groups),
        _ => (AHashMap::new(), AHashMap::new()),
    };

    let mut out: Vec<DocHit> = groups
        .iter()
        .map(|group| {
            let meta = metas.get(&group.first.doc);
            hit_of(group, meta.and_then(|m| m.0.clone()), meta.map(|m| m.1))
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
    // Context goes on in rank order while the budget holds: before, then after.
    for (hit, group) in out.iter_mut().zip(&groups) {
        let (ord, ord_end) = group.ord_range();
        let before = ord
            .checked_sub(1)
            .and_then(|o| neighbours.get(&(group.first.doc.clone(), o)));
        let after = neighbours.get(&(group.first.doc.clone(), ord_end + 1));
        if let Some(chunk) = before {
            let cost = bytes_to_tokens(chunk.text.len() as u64);
            if budget.is_none_or(|budget| used + cost <= budget) {
                used += cost;
                hit.text = stitch(&chunk.text, &hit.text);
                hit.context.before = 1;
            }
        }
        if let Some(chunk) = after {
            let cost = bytes_to_tokens(chunk.text.len() as u64);
            if budget.is_none_or(|budget| used + cost <= budget) {
                used += cost;
                hit.text = stitch(&hit.text, &chunk.text);
                hit.context.after = 1;
            }
        }
    }

    // The page ends where its last kept hit's members end in the ranking.
    let kept_members: u32 = groups.iter().take(kept).map(|g| g.members.len() as u32).sum();
    let next = offset + if budgeted { kept_members } else { consumed };
    let more = !out.is_empty() && next < fetched;
    let truncated_reason = truncated_reason.or_else(|| goes_on.then(|| "depth".to_string()));
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

type Metas = AHashMap<String, (Option<String>, u32)>;
type Neighbours = AHashMap<(String, u32), DocChunkHit>;

/// Titles and chunk counts of the page's documents, and the chunks on either side of each group.
/// A store error leaves the page without context rather than failing it.
fn context_of(store: &DocStore, groups: &[Merged]) -> (Metas, Neighbours) {
    let mut keys: Vec<String> = groups.iter().map(|g| g.first.doc.clone()).collect();
    keys.sort_unstable();
    keys.dedup();
    let mut wanted: AHashMap<String, Vec<u32>> = AHashMap::new();
    for group in groups {
        let (ord, ord_end) = group.ord_range();
        let ords = wanted.entry(group.first.doc.clone()).or_default();
        if let Some(before) = ord.checked_sub(1) {
            ords.push(before);
        }
        ords.push(ord_end + 1);
    }
    let wanted: Vec<(String, Vec<u32>)> = wanted.into_iter().collect();
    let fetched = store
        .documents(&keys)
        .and_then(|metas| store.chunks(&wanted).map(|chunks| (metas, chunks)));
    match fetched {
        Ok((metas, chunks)) => (
            metas
                .into_iter()
                .map(|meta| (meta.doc_key, (meta.title, meta.n_chunks)))
                .collect(),
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
