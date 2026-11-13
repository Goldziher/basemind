//! Layout and result projection for content-addressed Lance tables.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const MAX_CANDIDATES: usize = 4096;

/// Model and dimensionality partition shared tables without exposing model strings as paths.
pub fn shared_directory(group: &Path, model: &str, dim: u16) -> PathBuf {
    let model = blake3::hash(model.as_bytes());
    group.join("lance").join(format!("{model}-{dim}"))
}

/// All effective paths for each hash, for exactly one lane and workspace snapshot.
#[derive(Default)]
pub struct LivePaths<'a> {
    by_hash: BTreeMap<Cow<'a, str>, BTreeSet<&'a str>>,
}

impl<'a> LivePaths<'a> {
    #[cfg(feature = "code-search")]
    pub fn from_code_entries(entries: impl IntoIterator<Item = (&'a str, &'a crate::store::FileEntry)>) -> Self {
        let mut paths = Self::default();
        for (path, entry) in entries {
            paths
                .by_hash
                .entry(Cow::Owned(code_content_key(entry)))
                .or_default()
                .insert(path);
        }
        paths
    }

    pub(super) fn paths_for(&self, hash: &str) -> impl Iterator<Item = &'a str> + '_ {
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

#[cfg(feature = "code-search")]
pub fn code_content_key(entry: &crate::store::FileEntry) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(&entry.extract_epoch.to_le_bytes());
    hash.update(&(entry.hash_hex.len() as u64).to_le_bytes());
    hash.update(entry.hash_hex.as_bytes());
    hash.update(entry.language.as_bytes());
    hash.finalize().to_hex().to_string()
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
/// shared-table query is approximate when the candidate cap excludes live rows.
pub fn search_live<'a, T: Clone, E>(
    paths: &LivePaths<'a>,
    limit: usize,
    mut fetch: impl FnMut(usize) -> Result<Vec<ContentHit<T>>, E>,
) -> Result<Vec<PathHit<'a, T>>, E> {
    if limit == 0 || paths.by_hash.is_empty() {
        return Ok(Vec::new());
    }
    let mut count = limit
        .saturating_mul(3)
        .max(limit.saturating_add(64))
        .min(MAX_CANDIDATES);
    loop {
        let rows = fetch(count)?;
        let full = rows.len() >= count;
        let mut hits = project(paths, &rows);
        if hits.len() >= limit || !full || count == MAX_CANDIDATES {
            hits.truncate(limit);
            return Ok(hits);
        }
        count = (count * 2).min(MAX_CANDIDATES);
    }
}

fn project<'a, T: Clone>(paths: &LivePaths<'a>, rows: &[ContentHit<T>]) -> Vec<PathHit<'a, T>> {
    let mut hits = Vec::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        if seen.insert((&row.hash, row.chunk_idx)) {
            hits.extend(paths.paths_for(&row.hash).map(|path| PathHit {
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
    fn shared_identity_isolates_models_and_dimensions() {
        let group = Path::new("group");
        let path = shared_directory(group, "../model", 384);
        assert_eq!(path.parent().unwrap(), group.join("lance"));
        assert_ne!(path, shared_directory(group, "../model", 768));
        assert_ne!(path, shared_directory(group, "model", 384));
    }

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
        assert_eq!(requests, [66, 132]);
        assert_eq!(
            hits.iter().map(|hit| (hit.path, hit.chunk_idx)).collect::<Vec<_>>(),
            [("a.rs", 3), ("z.rs", 3)]
        );
    }
}
