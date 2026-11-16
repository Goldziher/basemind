//! A small cache of ranked retrievals, for `code_search` and `docs_search`.
//!
//! Every page of a ranked search is cut from one retrieval of the whole paging depth, and that
//! retrieval (the query embedding, the vector and keyword lanes, the cross-encoder over every
//! candidate) costs the same whichever page is asked for. The ranked list is kept here, so the
//! later pages of a query and a repeated call are cut from the list the first call built.
//!
//! An entry answers only the query it was built for, at the index generation it was built at: a
//! rescan bumps the generation and every older entry is dropped. A ranking also depends on state
//! the generation does not count (chunks still being embedded after a scan, a page scraped into
//! the document store), so an entry is served for [`MAX_AGE`] at most.

#![cfg_attr(
    not(any(feature = "code-search", feature = "documents")),
    allow(
        dead_code,
        reason = "compiled for its tests when neither ranked tool is in the build"
    )
)]

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

/// Rankings kept at once.
const ENTRIES_MAX: usize = 16;
/// Serialized bytes of every ranking kept, together. One ranking is up to 101 hits with their
/// snippets or chunk text.
const BYTES_MAX: usize = 4 * 1024 * 1024;
/// How long a ranking is served before the retrieval runs again.
const MAX_AGE: Duration = Duration::from_secs(120);

struct Entry {
    key: String,
    generation: u32,
    stored: Instant,
    bytes: usize,
    body: Value,
}

/// Ranked lists by query, least recently used first. Bounded by [`ENTRIES_MAX`] and [`BYTES_MAX`].
#[derive(Default)]
pub(crate) struct RankedCache {
    entries: Mutex<VecDeque<Entry>>,
}

impl RankedCache {
    /// The identity of a retrieval: the tool and every argument that shapes its ranking. `limit`,
    /// `cursor` and `max_tokens` only choose a page of it and are left out.
    pub(super) fn key(tool: &str, arguments: &Value) -> String {
        format!("{tool} {arguments}")
    }

    /// The ranking stored for `key` at `generation`, if it is still served.
    pub(super) fn get(&self, key: &str, generation: u32) -> Option<Value> {
        self.get_at(key, generation, Instant::now())
    }

    /// Keep `body` as the ranking of `key` at `generation`. A body larger than the whole byte
    /// bound is not kept.
    pub(super) fn put(&self, key: String, generation: u32, body: &Value) {
        self.put_at(key, generation, body, Instant::now());
    }

    fn get_at(&self, key: &str, generation: u32, now: Instant) -> Option<Value> {
        let mut entries = self.entries.lock().ok()?;
        // A lookup that started before a rescan asks with the older generation: it drops what is
        // older still, and leaves the newer generation's rankings to the calls that will want them.
        entries.retain(|entry| entry.generation >= generation && now.duration_since(entry.stored) < MAX_AGE);
        let at = entries
            .iter()
            .position(|entry| entry.generation == generation && entry.key == key)?;
        let entry = entries.remove(at)?;
        let body = entry.body.clone();
        entries.push_back(entry);
        Some(body)
    }

    fn put_at(&self, key: String, generation: u32, body: &Value, now: Instant) {
        let Ok(bytes) = serde_json::to_vec(body).map(|encoded| encoded.len()) else {
            return;
        };
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        // A retrieval that started before a rescan finishes after it: keeping its ranking would
        // evict the newer generation's, and the ranking would never be served.
        if entries.iter().any(|entry| entry.generation > generation) {
            return;
        }
        entries.retain(|entry| entry.generation == generation && entry.key != key);
        if bytes > BYTES_MAX {
            return;
        }
        entries.push_back(Entry {
            key,
            generation,
            stored: now,
            bytes,
            body: body.clone(),
        });
        let mut held: usize = entries.iter().map(|entry| entry.bytes).sum();
        while entries.len() > ENTRIES_MAX || held > BYTES_MAX {
            let Some(evicted) = entries.pop_front() else {
                break;
            };
            held -= evicted.bytes;
        }
    }

    #[cfg(test)]
    fn held(&self) -> (usize, usize) {
        let entries = self.entries.lock().expect("lock");
        (entries.len(), entries.iter().map(|entry| entry.bytes).sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ranking(tag: &str) -> Value {
        json!({ "hits": [{ "path": tag }] })
    }

    fn key(query: &str) -> String {
        RankedCache::key("code_search", &json!({ "query": query, "lane": null }))
    }

    #[test]
    fn a_stored_ranking_answers_its_own_query_and_no_other() {
        let cache = RankedCache::default();
        assert_eq!(cache.get(&key("retry"), 3), None);
        cache.put(key("retry"), 3, &ranking("a.rs"));
        assert_eq!(cache.get(&key("retry"), 3), Some(ranking("a.rs")));
        assert_eq!(cache.get(&key("upload"), 3), None, "another query");
        let docs = RankedCache::key("docs_search", &json!({ "query": "retry", "lane": null }));
        assert_eq!(cache.get(&docs, 3), None, "another tool");
        let laned = RankedCache::key("code_search", &json!({ "query": "retry", "lane": "keyword" }));
        assert_eq!(cache.get(&laned, 3), None, "another lane ranks differently");
    }

    #[test]
    fn a_rescan_drops_every_ranking_of_the_older_generation() {
        let cache = RankedCache::default();
        cache.put(key("retry"), 3, &ranking("a.rs"));
        cache.put(key("upload"), 3, &ranking("b.rs"));
        assert_eq!(cache.get(&key("retry"), 4), None, "the index changed");
        assert_eq!(cache.held().0, 0, "and nothing of generation 3 is kept");

        cache.put(key("retry"), 4, &ranking("c.rs"));
        cache.put(key("upload"), 5, &ranking("d.rs"));
        assert_eq!(cache.held().0, 1, "a put at a newer generation drops the older ones");
        assert_eq!(cache.get(&key("upload"), 5), Some(ranking("d.rs")));
    }

    #[test]
    fn a_ranking_of_an_older_generation_evicts_nothing() {
        let cache = RankedCache::default();
        cache.put(key("retry"), 5, &ranking("new.rs"));
        cache.put(key("upload"), 4, &ranking("old.rs"));
        assert_eq!(
            cache.get(&key("retry"), 5),
            Some(ranking("new.rs")),
            "a put that finished after the rescan keeps the newer ranking"
        );
        assert_eq!(cache.held().0, 1, "and the older one is not kept");
    }

    #[test]
    fn a_lookup_at_an_older_generation_evicts_nothing_newer() {
        let cache = RankedCache::default();
        cache.put(key("retry"), 5, &ranking("new.rs"));
        assert_eq!(cache.get(&key("retry"), 4), None, "a lookup from before the rescan");
        assert_eq!(
            cache.get(&key("retry"), 5),
            Some(ranking("new.rs")),
            "leaves the newer generation's ranking in place"
        );
    }

    #[test]
    fn a_ranking_is_served_for_a_bounded_time() {
        let cache = RankedCache::default();
        let stored = Instant::now();
        cache.put_at(key("retry"), 3, &ranking("a.rs"), stored);
        let young = stored + MAX_AGE - Duration::from_secs(1);
        assert_eq!(cache.get_at(&key("retry"), 3, young), Some(ranking("a.rs")));
        assert_eq!(cache.get_at(&key("retry"), 3, stored + MAX_AGE), None);
    }

    #[test]
    fn the_cache_holds_a_bounded_number_of_rankings_and_bytes() {
        let cache = RankedCache::default();
        for query in 0..ENTRIES_MAX + 5 {
            cache.put(key(&query.to_string()), 3, &ranking("a.rs"));
        }
        assert_eq!(cache.held().0, ENTRIES_MAX);
        assert_eq!(cache.get(&key("0"), 3), None, "the least recently used went first");
        assert!(cache.get(&key(&(ENTRIES_MAX + 4).to_string()), 3).is_some());

        // Reading an entry keeps it: the next eviction takes the one after it.
        assert!(cache.get(&key("5"), 3).is_some());
        cache.put(key("one more"), 3, &ranking("a.rs"));
        assert!(cache.get(&key("5"), 3).is_some());
        assert_eq!(cache.get(&key("6"), 3), None);

        let big = json!({ "hits": ["x".repeat(BYTES_MAX / 3)] });
        let cache = RankedCache::default();
        for query in 0..4 {
            cache.put(key(&query.to_string()), 3, &big);
        }
        let (entries, bytes) = cache.held();
        assert_eq!(entries, 2, "three of them pass the byte bound");
        assert!(bytes <= BYTES_MAX);

        let oversized = json!({ "hits": ["x".repeat(BYTES_MAX + 1)] });
        cache.put(key("oversized"), 3, &oversized);
        assert_eq!(
            cache.get(&key("oversized"), 3),
            None,
            "one ranking over the whole bound"
        );
        assert_eq!(cache.held().0, 2, "and it evicted nothing");
    }
}
