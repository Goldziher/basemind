//! `memory_search` by words, for a query no embedding model can embed (none is downloaded, the
//! machine is offline, the model refused the text).
//!
//! The query and every note of the namespace are split into lowercase words. A note matches when
//! at least half of the query's words each begin one of the note's words, in its key, value or
//! tags: `token rotation` finds `Tokens rotate every 24h.` through `token`. Hits rank by how many
//! of the query's words they hold, then by key, and carry as `distance` the share of the query's
//! words they lack, so a hit holding every word is at `0`.

use std::time::Instant;

use ahash::AHashSet;

use super::ops::WireMemoryEntry;
use super::vectors::whole_notes;
use crate::engine::timing::elapsed_us;
use crate::engine::types::memory::{MemorySearchHit, MemorySearchParams, MemorySearchResponse};
use crate::engine::{EngineError, Workspace};

/// Notes listed per page while matching.
const PAGE: usize = 500;

/// The lowercase words of `text`: its runs of letters and digits, each once.
pub(crate) fn words(text: &str) -> Vec<String> {
    let mut seen = AHashSet::new();
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .filter(|word| seen.insert(word.clone()))
        .collect()
}

/// How many of `query` (lowercase words) begin a word of `note`'s key, value or tags.
pub(crate) fn matched(query: &[String], note: &WireMemoryEntry) -> usize {
    let mut text = words(&note.key);
    text.extend(words(&note.value));
    for tag in &note.tags {
        text.extend(words(tag));
    }
    query
        .iter()
        .filter(|word| text.iter().any(|own| own.starts_with(word.as_str())))
        .count()
}

/// Whether a note holding `matched` of `of` query words is a hit: at least half of them.
pub(crate) fn is_hit(matched: usize, of: usize) -> bool {
    matched > 0 && matched * 2 >= of
}

/// What the reply says about a search that matched words, `reason` being why no model embedded
/// the query.
fn notice(reason: &str) -> String {
    format!(
        "no embedding model could embed the query ({reason}), so the notes were matched by its words: a hit \
         holds at least half of them, and `distance` is the share it lacks"
    )
}

/// `memory_search` by words over every note of the namespace, whole values read for the notes a
/// listing clipped. `reason` is why the query was not embedded.
pub(crate) fn search(
    ws: &Workspace,
    params: MemorySearchParams,
    reason: &str,
    started: Instant,
) -> Result<MemorySearchResponse, EngineError> {
    let limit = params.limit.unwrap_or(10).min(100) as usize;
    let (vis_byte, owner) = ws.memory_namespace(params.visibility);
    let query = words(&params.query);
    let mut scored: Vec<(usize, WireMemoryEntry)> = Vec::new();
    let mut cursor = None;
    loop {
        let page = ws.memory_list_page(vis_byte, owner, "", &params.tags, PAGE, cursor)?;
        for note in whole_notes(ws, vis_byte, owner, &page.entries)? {
            let held = matched(&query, &note);
            if is_hit(held, query.len()) {
                scored.push((held, note));
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    scored.sort_by(|(a, left), (b, right)| b.cmp(a).then_with(|| left.key.cmp(&right.key)));
    let hits = scored
        .into_iter()
        .take(limit)
        .map(|(held, note)| MemorySearchHit {
            key: note.key,
            value: super::ops::preview(&note.value),
            tags: note.tags,
            distance: lacking(held, query.len()),
        })
        .collect();
    Ok(MemorySearchResponse {
        query: params.query,
        hits,
        lexical: true,
        notice: Some(notice(reason)),
        elapsed_us: elapsed_us(started),
    })
}

/// The share of `of` query words a hit holding `held` of them lacks.
fn lacking(held: usize, of: usize) -> f32 {
    1.0 - held as f32 / of as f32
}

#[cfg(test)]
#[path = "lexical_tests.rs"]
mod tests;
