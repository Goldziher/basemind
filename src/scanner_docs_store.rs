//! The scan's side of the document retrieval store (ADR-0029 rules 2 and 11, ADR-0037 rule 3):
//! bring the repository group's store in line with the view's document map.
//!
//! Each document of the map has a `doc_key`, computed from the content hash and extraction digest
//! its entry records. The scan asks the store which keys are absent, reads blobs only for those,
//! and writes their rows; a document another worktree already wrote is not written again. Rows of
//! keys no view of the group names are deleted ([`Store::group_live_doc_keys`]); while that set is
//! unknown nothing is. A wiped or deleted store is rebuilt by the next scan from the cached blobs,
//! with no embedding.

use ahash::{AHashMap, AHashSet};

use crate::config::Config;
use crate::extract::doc::{DocBlobs, FileMapDoc};
use crate::lance::docs::{DocStore, doc_key, group_store_dir, mime_group};
use crate::lance::docs_schema::{DocChunkRow, DocRows, SECTION_SEPARATOR};
use crate::scanner_filter::Filters;
use crate::store::Store;

/// Documents per store write; a write holds their rows in memory.
const WRITE_DOCS: usize = crate::lance::BATCH_MAX_DOCS;
/// Chunk rows per store write.
const WRITE_ROWS: usize = crate::lance::BATCH_MAX_ROWS;

/// The group's retrieval store for the configured preset. With `create` false, `None` when no store
/// was ever written for it: removing rows must not create one.
///
/// Every open (all of them are a scan's) also removes the view's own stores an earlier build kept
/// under `views/<view>/retrieval/`, whether or not the view still holds documents: the group store
/// replaces them, and is filled from the cached blobs without embedding again.
pub(crate) fn open_doc_store(store: &Store, config: &Config, create: bool) -> Result<Option<DocStore>, String> {
    let legacy = store.view_dir.join(crate::store_layout::RETRIEVAL_DIR);
    if legacy.is_dir()
        && let Err(error) = std::fs::remove_dir_all(&legacy)
    {
        tracing::debug!(%error, dir = %legacy.display(), "removing the view's old document store failed");
    }
    let preset = &config.documents.embedding_preset;
    let dim = super::preset_dim(preset).map_err(|error| error.to_string())?;
    let dir = group_store_dir(&store.root, preset, dim);
    let opened = if create {
        DocStore::shared(&dir, dim, preset).map(Some)
    } else {
        DocStore::shared_existing(&dir, dim, preset)
    };
    opened.map_err(|error| format!("{error:#}"))
}

/// One live document: the blob its rows are built from, its paths, and whether any of them wants
/// vectors.
struct LiveDoc {
    blob: String,
    paths: Vec<String>,
    want_vectors: bool,
    /// The entry says the blob holds vectors of the preset its scan asked for.
    embedded: bool,
}

fn live_docs(store: &Store, config: &Config, filters: &Filters) -> AHashMap<String, LiveDoc> {
    let cfg = &config.documents;
    let mut live: AHashMap<String, LiveDoc> = AHashMap::new();
    for (path, entry) in &store.index.doc_files {
        let path = path.to_str_lossy().into_owned();
        let want = cfg.embed && filters.doc_embed_allowed(&path);
        let doc = live
            .entry(doc_key(&entry.hash_hex, &entry.config_digest))
            .or_insert_with(|| LiveDoc {
                blob: entry.hash_hex.clone(),
                paths: Vec::new(),
                want_vectors: false,
                embedded: false,
            });
        doc.want_vectors |= want;
        doc.embedded |= entry.embedded && entry.embedding_preset == cfg.embedding_preset;
        doc.paths.push(path);
    }
    live
}

/// The rows of one document. Over `max_chunks` chunks only the document row is written (no chunk
/// row, as before: such a document is tracked but not searched). A chunk carries its vector when
/// `with_vectors` and the vector is whole.
pub(crate) fn build_doc_rows(doc: FileMapDoc, key: String, with_vectors: bool, max_chunks: usize) -> DocRows {
    let title = doc
        .metadata
        .iter()
        .find(|(name, value)| {
            (name.eq_ignore_ascii_case("title") || name.eq_ignore_ascii_case("subject")) && !value.trim().is_empty()
        })
        .map(|(_, value)| value.trim().to_string());
    let mut keywords: Vec<String> = doc.keywords.iter().map(|k| k.text.trim().to_lowercase()).collect();
    keywords.sort_unstable();
    keywords.dedup();
    let dim = usize::from(doc.embedding_dim);
    let over_cap = doc.chunks.len() > max_chunks;
    let entities = &doc.entities;
    let chunks = if over_cap {
        Vec::new()
    } else {
        doc.chunks
            .into_iter()
            .enumerate()
            .map(|(ord, chunk)| {
                let mut entity_cats: Vec<String> = entities
                    .iter()
                    .filter(|e| e.start < chunk.byte_end && e.end > chunk.byte_start)
                    .map(|e| e.category.to_lowercase())
                    .collect();
                entity_cats.sort_unstable();
                entity_cats.dedup();
                let embedding = (with_vectors && dim > 0 && chunk.embedding.len() == dim).then_some(chunk.embedding);
                DocChunkRow {
                    chunk_ord: u32::try_from(ord).unwrap_or(u32::MAX),
                    chunk_kind: if chunk.kind.is_empty() {
                        "unknown".to_string()
                    } else {
                        chunk.kind
                    },
                    page_start: chunk.page_start,
                    page_end: chunk.page_end,
                    section: (!chunk.section.is_empty()).then(|| chunk.section.join(SECTION_SEPARATOR)),
                    line_start: chunk.line_start,
                    line_end: chunk.line_end,
                    byte_start: chunk.byte_start,
                    byte_end: chunk.byte_end,
                    entity_cats,
                    text: chunk.text,
                    embedding,
                }
            })
            .collect()
    };
    DocRows {
        doc_key: key,
        mime_group: mime_group(&doc.mime_type).to_string(),
        mime_type: doc.mime_type,
        title,
        keywords,
        chunks,
    }
}

/// Bring the store in line with the view's document map: write the rows of every live document
/// the store lacks (or holds without the vectors it now has), settle their paths in the owed set,
/// delete the rows no live document names, and bring the indexes up to date. Returns the number
/// of documents written. Failures are logged; what was not written stays owed.
///
/// Runs after the pass wrote its file map, which then names every key written here: a sibling's
/// sweep may delete a row whose key no file map on disk names ([`DocStore::write`]).
///
/// The documents at `scanned` (this scan's) are always written when absent; at most `budget` other
/// absent documents are, so an incremental or cancelled pass does not read every blob a wiped
/// store lacks. The rest are recorded as owed for a later pass.
///
/// A full pass ([`crate::scanner_flush::REQUEUE_ALL`]) sweeps every dead row; any other pass checks
/// only the keys it dropped ([`Store::dropped_doc_keys`]), and reads the other views' file maps
/// only when one of them is not in this view's own.
pub(crate) fn sync_document_rows(
    store: &mut Store,
    config: &Config,
    filters: &Filters,
    scanned: &AHashSet<String>,
    budget: usize,
) -> usize {
    let dropped = std::mem::take(&mut store.dropped_doc_keys);
    let live = live_docs(store, config, filters);
    let doc_store = match open_doc_store(store, config, !live.is_empty()) {
        Ok(Some(doc_store)) => doc_store,
        Ok(None) => return 0,
        Err(error) => {
            tracing::warn!(%error, "document store did not open; document rows stay owed");
            return 0;
        }
    };
    let keys: Vec<String> = live.keys().cloned().collect();
    let present = match doc_store.present(&keys) {
        Ok(present) => present,
        Err(error) => {
            tracing::warn!(?error, "listing the documents the store holds failed; rows stay owed");
            return 0;
        }
    };
    let absent: Vec<&String> = keys
        .iter()
        .filter(|key| match present.get(*key) {
            None => true,
            Some(&has_vectors) => !has_vectors && live[*key].want_vectors && live[*key].embedded,
        })
        .collect();
    let (mut todo, others): (Vec<&String>, Vec<&String>) = absent
        .into_iter()
        .partition(|key| live[*key].paths.iter().any(|path| scanned.contains(path)));
    let (within, beyond) = others.split_at(others.len().min(budget));
    todo.extend_from_slice(within);
    for key in beyond {
        for path in &live[*key].paths {
            store.pending_vectors.owe_doc(path);
        }
    }
    let preset = config.documents.embedding_preset.clone();
    let max_chunks = config.documents.max_chunks_per_document;
    let mut written = 0usize;
    // Paths whose rows are in the store and need no rewrite owe nothing; the rest settle once written.
    let queued: AHashSet<&String> = todo.iter().copied().chain(beyond.iter().copied()).collect();
    let mut settled: Vec<String> = keys
        .iter()
        .filter(|key| present.contains_key(*key) && !queued.contains(key))
        .flat_map(|key| live[key].paths.iter().cloned())
        .collect();
    drop(queued);
    let mut blobless: Vec<String> = Vec::new();
    let mut damaged: Vec<String> = Vec::new();
    let mut pending: Vec<DocRows> = Vec::new();
    let mut pending_paths: Vec<String> = Vec::new();
    let mut rows = 0usize;
    let mut flush = |pending: &mut Vec<DocRows>, pending_paths: &mut Vec<String>, rows: &mut usize| {
        if pending.is_empty() {
            return;
        }
        match doc_store.write(pending) {
            Ok(()) => {
                written += pending.len();
                settled.append(pending_paths);
            }
            Err(error) => {
                tracing::warn!(
                    docs = pending.len(),
                    ?error,
                    "writing document rows failed; they stay owed"
                );
                pending_paths.clear();
            }
        }
        pending.clear();
        *rows = 0;
    };
    for key in todo {
        let doc = &live[key];
        match store.read_doc_by_hex(&doc.blob) {
            Ok(Some(blob)) => {
                let with_vectors = doc.want_vectors && super::doc_has_vectors_of(&blob, &preset);
                let built = build_doc_rows(blob, key.clone(), with_vectors, max_chunks);
                rows += built.chunks.len();
                pending.push(built);
                pending_paths.extend(doc.paths.iter().cloned());
                if pending.len() >= WRITE_DOCS || rows >= WRITE_ROWS {
                    flush(&mut pending, &mut pending_paths, &mut rows);
                }
            }
            // No blob: the entry's next extraction writes one; nothing to build rows from now, so
            // nothing is owed until then.
            Ok(None) => {
                tracing::debug!(
                    key,
                    "no blob for a live document; its rows wait for its next extraction"
                );
                blobless.extend(doc.paths.iter().cloned());
            }
            Err(crate::store::StoreError::Io { source, .. }) => {
                tracing::warn!(key, error = %source, "reading a document blob failed; its rows stay owed");
            }
            // A blob that does not decode (torn by a power loss) would be skipped as unchanged for
            // good: its entry is dropped so the next pass extracts the file again.
            Err(error) => {
                tracing::warn!(key, %error, "document blob damaged; the file is extracted again");
                damaged.extend(doc.paths.iter().cloned());
            }
        }
    }
    flush(&mut pending, &mut pending_paths, &mut rows);
    drop(flush);
    settled.append(&mut blobless);
    // An owed path the view no longer tracks has no rows to come.
    let tracked: AHashSet<&str> = live
        .values()
        .flat_map(|doc| doc.paths.iter().map(String::as_str))
        .collect();
    settled.extend(
        store
            .pending_vectors
            .docs()
            .iter()
            .map(|rel| rel.to_str_lossy().into_owned())
            .filter(|rel| !tracked.contains(rel.as_str())),
    );
    drop(tracked);
    store.pending_vectors.settle_docs(&settled);
    for path in &damaged {
        store.pending_vectors.owe_doc(path);
    }
    crate::scanner_flush::extract_again(store, &damaged, |store, rel| store.remove_doc(rel));
    if budget == crate::scanner_flush::REQUEUE_ALL {
        drop(live);
        sweep_dead_rows(store, &doc_store);
    } else {
        sweep_dropped_rows(store, &doc_store, dropped, |key| live.contains_key(key));
        drop(live);
    }
    if let Err(error) = doc_store.maintain_indexes() {
        tracing::warn!(?error, "document index upkeep failed; searches scan the unindexed rows");
    }
    written
}

/// Delete the rows of the group's store whose key no view of the group names. A key one view
/// dropped stays while another names it. When a view's document map cannot be read the liveness is
/// unknown and nothing is deleted; dead rows are filtered out at query time meanwhile.
fn sweep_dead_rows(store: &Store, doc_store: &DocStore) {
    sweep_dead_rows_as_of(doc_store, || store.group_live_doc_keys());
}

/// [`sweep_dead_rows`] with the live keys read by `live`, under the store's sweep gate: a sibling
/// that names a document after `live` read its file map writes the rows after the delete.
fn sweep_dead_rows_as_of(
    doc_store: &DocStore,
    live: impl FnOnce() -> Result<AHashSet<String>, crate::store::LivenessUnknown>,
) {
    let _exclusive = doc_store.sweep_exclusive();
    let live_keys = match live() {
        Ok(keys) => keys,
        Err(error) => {
            tracing::warn!(%error, "the group's live documents are unknown; no document rows are deleted");
            return;
        }
    };
    if let Err(error) = doc_store.delete_dead(&live_keys) {
        tracing::warn!(
            ?error,
            "deleting dead document rows failed; they are filtered out at query time"
        );
    }
}

/// Delete the rows of the keys `dropped` names that no view of the group names any more. The other
/// views' file maps are read only when this view's own map names none of them. A dead row of a key
/// not dropped here waits for the next full pass ([`sweep_dead_rows`]).
fn sweep_dropped_rows(
    store: &Store,
    doc_store: &DocStore,
    mut dropped: Vec<String>,
    own_names: impl Fn(&String) -> bool,
) {
    dropped.retain(|key| !own_names(key));
    if dropped.is_empty() {
        return;
    }
    dropped.sort_unstable();
    dropped.dedup();
    let _exclusive = doc_store.sweep_exclusive();
    let live = match store.group_live_doc_keys() {
        Ok(keys) => keys,
        Err(error) => {
            tracing::warn!(%error, "the group's live documents are unknown; no document rows are deleted");
            return;
        }
    };
    dropped.retain(|key| !live.contains(key));
    if let Err(error) = doc_store.delete_keys(&dropped) {
        tracing::warn!(
            ?error,
            "deleting dropped document rows failed; they are filtered out at query time"
        );
    }
}

/// Delete the rows of the documents this pass dropped that no view of the group names any more.
/// No store is created for it.
pub(crate) fn remove_dead_document_rows(store: &mut Store, config: &Config) {
    let dropped = std::mem::take(&mut store.dropped_doc_keys);
    if dropped.is_empty() {
        return;
    }
    let own: AHashSet<String> = store
        .index
        .doc_files
        .values()
        .map(|entry| doc_key(&entry.hash_hex, &entry.config_digest))
        .collect();
    match open_doc_store(store, config, false) {
        Ok(Some(doc_store)) => sweep_dropped_rows(store, &doc_store, dropped, |key| own.contains(key)),
        Ok(None) => {}
        Err(error) => tracing::warn!(%error, "document store did not open; removed documents' rows stay"),
    }
}

/// Rewrite without vectors the rows of documents no path of which may be embedded any more. They
/// stay searchable by the lexical lane. A document another view of the group names keeps its
/// vectors: that view's policy may want them, and the rows are shared (ADR-0037 rule 3). `false`
/// when rows that carry vectors could not be rewritten, or the other views' documents are unknown.
pub(crate) fn strip_unwanted_vectors(store: &Store, config: &Config, filters: &Filters) -> bool {
    let live = live_docs(store, config, filters);
    let mut unwanted: Vec<String> = live
        .iter()
        .filter(|(_, doc)| !doc.want_vectors)
        .map(|(key, _)| key.clone())
        .collect();
    if unwanted.is_empty() {
        return true;
    }
    match store.sibling_doc_keys() {
        Ok(siblings) => unwanted.retain(|key| !siblings.contains(key)),
        Err(error) => {
            tracing::warn!(%error, "the group's other documents are unknown; excluded documents keep their vectors");
            return false;
        }
    }
    if unwanted.is_empty() {
        return true;
    }
    let doc_store = match open_doc_store(store, config, false) {
        Ok(Some(doc_store)) => doc_store,
        Ok(None) => return true,
        Err(error) => {
            tracing::warn!(%error, "document store did not open; excluded documents keep their vectors");
            return false;
        }
    };
    let present = match doc_store.present(&unwanted) {
        Ok(present) => present,
        Err(error) => {
            tracing::warn!(?error, "listing documents with vectors failed");
            return false;
        }
    };
    let max_chunks = config.documents.max_chunks_per_document;
    let mut rewrite = Vec::new();
    for key in unwanted.iter().filter(|key| present.get(*key) == Some(&true)) {
        if let Ok(Some(blob)) = store.read_doc_by_hex(&live[key].blob) {
            rewrite.push(build_doc_rows(blob, key.clone(), false, max_chunks));
        }
    }
    let mut applied = true;
    for part in rewrite.chunks(WRITE_DOCS) {
        if let Err(error) = doc_store.write(part) {
            tracing::warn!(?error, "rewriting rows without vectors failed");
            applied = false;
        }
    }
    applied
}

#[cfg(test)]
#[path = "scanner_docs_store_tests.rs"]
mod tests;
