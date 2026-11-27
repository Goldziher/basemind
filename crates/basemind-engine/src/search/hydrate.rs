//! Turn ranked code-search chunk ids into chunks and the paths that hold them now.
//!
//! A chunk id is `<content-hash>:<ordinal>`, and its sidecar is shared by every file with that
//! content, so the path inside the sidecar is only the path that was chunked first. The paths a hit
//! cites come from the view's file map at query time instead: a renamed file is cited at its new
//! path, and identical files are one hit listing every path. Each sidecar is decoded at most once
//! per query, without its embeddings.

use ahash::{AHashMap, AHashSet};

use crate::chunk::CodeChunk;
use crate::store::Store;

/// Resolves chunk ids for one query against one store.
pub struct ChunkHydrator<'a> {
    store: &'a Store,
    chunks: AHashMap<String, Option<Vec<CodeChunk>>>,
    live: AHashMap<String, Vec<String>>,
    decodes: usize,
}

/// The content-hash half of a chunk id.
fn hash_of(chunk_id: &str) -> Option<&str> {
    chunk_id.rsplit_once(':').map(|(hash, _)| hash)
}

impl<'a> ChunkHydrator<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self {
            store,
            chunks: AHashMap::new(),
            live: AHashMap::new(),
            decodes: 0,
        }
    }

    /// Find, in one pass over the file map, every live path holding the content of each id. Paths
    /// are sorted, so the cited path of a duplicated file is stable.
    pub fn resolve_paths<'i>(&mut self, chunk_ids: impl IntoIterator<Item = &'i str>) {
        let wanted: AHashSet<&str> = chunk_ids
            .into_iter()
            .filter_map(hash_of)
            .filter(|hash| !self.live.contains_key(*hash))
            .collect();
        if wanted.is_empty() {
            return;
        }
        let mut found: AHashMap<String, Vec<String>> = wanted.iter().map(|h| ((*h).to_string(), Vec::new())).collect();
        for (path, entry) in &self.store.index.files {
            if let Some(paths) = found.get_mut(entry.hash_hex.as_str()) {
                paths.push(path.to_string());
            }
        }
        for paths in found.values_mut() {
            paths.sort_unstable();
        }
        self.live.extend(found);
    }

    /// The live paths holding `chunk_id`'s content, sorted; empty when no indexed file holds it.
    /// Only ids passed to [`resolve_paths`](Self::resolve_paths) are known.
    pub fn paths(&self, chunk_id: &str) -> &[String] {
        hash_of(chunk_id)
            .and_then(|hash| self.live.get(hash))
            .map_or(&[], Vec::as_slice)
    }

    /// The chunk `chunk_id` names, decoding its sidecar on first use.
    pub fn chunk(&mut self, chunk_id: &str) -> Option<&CodeChunk> {
        let (hash, ordinal) = chunk_id.rsplit_once(':')?;
        let ordinal: usize = ordinal.parse().ok()?;
        if !self.chunks.contains_key(hash) {
            self.decodes += 1;
            let decoded = self
                .store
                .read_chunk_texts_by_hex(hash)
                .ok()
                .flatten()
                .map(|b| b.chunks);
            self.chunks.insert(hash.to_string(), decoded);
        }
        self.chunks.get(hash)?.as_ref()?.get(ordinal)
    }

    /// Sidecars decoded so far.
    pub fn decodes(&self) -> usize {
        self.decodes
    }
}
