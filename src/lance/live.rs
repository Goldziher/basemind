//! Liveness projection for content-keyed Lance rows (ADR-0028, ADR-0029): a search over rows keyed
//! by content keeps only those a view's file map holds, and expands each to the view's live paths.
//!
//! Moved out of the deleted shared Lance lane (ADR-0016) for the group vector store; nothing calls it
//! until that store lands.
#![cfg_attr(not(test), allow(dead_code))] // ~keep kept for the ADR-0029 group vector store

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

const MAX_CANDIDATES: usize = 4096;

/// All effective paths for each content key, for exactly one lane and view.
#[derive(Default)]
pub struct LivePaths<'a> {
    by_hash: BTreeMap<Cow<'a, str>, BTreeSet<&'a str>>,
    exact: BTreeMap<(&'a str, u32), BTreeSet<&'a str>>,
}

impl<'a> LivePaths<'a> {
    pub(crate) fn from_exact(entries: impl IntoIterator<Item = (&'a str, u32, &'a str)>) -> Self {
        let mut paths = Self::default();
        for (key, ordinal, path) in entries {
            paths.exact.entry((key, ordinal)).or_default().insert(path);
        }
        paths
    }

    fn paths_for_chunk<'b>(&'b self, hash: &'b str, ordinal: u32) -> impl Iterator<Item = &'a str> + 'b {
        self.exact
            .get(&(hash, ordinal))
            .into_iter()
            .flat_map(|paths| paths.iter().copied())
            .chain(self.paths_for(hash))
    }

    fn paths_for(&self, hash: &str) -> impl Iterator<Item = &'a str> + '_ {
        self.by_hash
            .get(hash)
            .into_iter()
            .flat_map(|paths| paths.iter().copied())
    }
}

impl<'a> FromIterator<(&'a str, &'a str)> for LivePaths<'a> {
    fn from_iter<T: IntoIterator<Item = (&'a str, &'a str)>>(entries: T) -> Self {
        let mut paths = Self::default();
        for (hash, path) in entries {
            paths.by_hash.entry(Cow::Borrowed(hash)).or_default().insert(path);
        }
        paths
    }
}

/// A ranked row from one scope of a shared table.
pub struct ContentHit<T> {
    pub hash: String,
    pub chunk_idx: u32,
    pub distance: f32,
    pub value: T,
}

/// The workspace path is authoritative even if the blob was written at a different path.
pub struct PathHit<'a, T> {
    pub path: &'a str,
    pub chunk_idx: u32,
    pub distance: f32,
    pub value: T,
}

/// Fetch increasing nearest-neighbor windows and project them onto one effective manifest.
///
/// Fetch must apply scope, model and any request filters before ranking. The bounded
/// shared-table query is approximate when the candidate cap excludes live rows. A full final window requires fallback.
pub fn search_live<'a, T: Clone, E>(
    paths: &LivePaths<'a>,
    limit: usize,
    mut fetch: impl FnMut(usize) -> Result<Vec<ContentHit<T>>, E>,
) -> Result<Option<Vec<PathHit<'a, T>>>, E> {
    if limit == 0 || (paths.by_hash.is_empty() && paths.exact.is_empty()) {
        return Ok(Some(Vec::new()));
    }
    let mut count = limit
        .saturating_mul(3)
        .max(limit.saturating_add(64))
        .min(MAX_CANDIDATES);
    loop {
        let rows = fetch(count)?;
        let full = rows.len() >= count;
        let mut hits = project(paths, &rows);
        if !full || (hits.len() >= limit && count < MAX_CANDIDATES) {
            hits.truncate(limit);
            return Ok(Some(hits));
        }
        if count == MAX_CANDIDATES {
            return Ok(None);
        }
        count = (count * 2).min(MAX_CANDIDATES);
    }
}

fn project<'a, T: Clone>(paths: &LivePaths<'a>, rows: &[ContentHit<T>]) -> Vec<PathHit<'a, T>> {
    let mut hits = Vec::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        if seen.insert((&row.hash, row.chunk_idx)) {
            hits.extend(paths.paths_for_chunk(&row.hash, row.chunk_idx).map(|path| PathHit {
                path,
                chunk_idx: row.chunk_idx,
                distance: row.distance,
                value: row.value.clone(),
            }));
        }
    }
    hits.sort_by(|a, b| {
        a.distance
            .total_cmp(&b.distance)
            .then(a.path.cmp(b.path))
            .then(a.chunk_idx.cmp(&b.chunk_idx))
    });
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_with_more_candidates_then_expands_and_orders_live_paths() {
        let paths = LivePaths::from_iter([("live", "z.rs"), ("live", "a.rs"), ("live", "a.rs")]);
        let mut requests = Vec::new();
        let hits = search_live(&paths, 2, |count| {
            requests.push(count);
            let mut rows = (0..66)
                .map(|chunk_idx| ContentHit {
                    hash: "stale".into(),
                    chunk_idx,
                    distance: 0.0,
                    value: (),
                })
                .collect::<Vec<_>>();
            if count > 66 {
                rows.push(ContentHit {
                    hash: "live".into(),
                    chunk_idx: 3,
                    distance: 1.0,
                    value: (),
                });
            }
            Ok::<_, ()>(rows)
        })
        .unwrap();
        let hits = hits.unwrap();
        assert_eq!(requests, [66, 132]);
        assert_eq!(
            hits.iter().map(|hit| (hit.path, hit.chunk_idx)).collect::<Vec<_>>(),
            [("a.rs", 3), ("z.rs", 3)]
        );
    }
    #[test]
    fn full_candidate_cap_requires_fallback() {
        let live = (0..10).map(|i| format!("{i}.rs")).collect::<Vec<_>>();
        let paths = LivePaths::from_iter(live.iter().map(|path| (path.as_str(), path.as_str())));
        let overlap = |stale: usize| {
            let hits = search_live(&paths, 10, |count| {
                let rows = (0..stale)
                    .map(|idx| ContentHit {
                        hash: "stale".into(),
                        chunk_idx: idx as u32,
                        distance: 0.0,
                        value: (),
                    })
                    .chain(live.iter().enumerate().map(|(idx, path)| ContentHit {
                        hash: path.clone(),
                        chunk_idx: 0,
                        distance: (idx + 1) as f32,
                        value: (),
                    }))
                    .take(count)
                    .collect();
                Ok::<_, ()>(rows)
            })
            .unwrap();
            hits.map(|hits| hits.len())
        };
        assert_eq!(overlap(100), Some(10));
        assert_eq!(overlap(MAX_CANDIDATES - 5), None);
        assert_eq!(overlap(MAX_CANDIDATES), None);
    }

    #[test]
    fn exact_rows_expand_only_to_the_paths_of_their_ordinal() {
        let paths = LivePaths::from_exact([("key", 0, "a.rs"), ("key", 1, "b.rs")]);
        let hits = search_live(&paths, 5, |_| {
            Ok::<_, ()>(vec![ContentHit {
                hash: "key".into(),
                chunk_idx: 1,
                distance: 0.5,
                value: (),
            }])
        })
        .unwrap()
        .unwrap();
        assert_eq!(hits.iter().map(|hit| hit.path).collect::<Vec<_>>(), ["b.rs"]);
    }
}
