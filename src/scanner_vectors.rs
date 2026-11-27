//! The LanceDB side of the scan: [`LanceVectors`] implements the core
//! [`VectorStore`](crate::scan_retrieval::VectorStore) seam, so the scan writes, deletes and maintains
//! vector rows without naming LanceDB or the embedding presets (ADR-0023 rule 1).

#![cfg(any(feature = "documents", feature = "code-search"))]

use crate::config::Config;
use crate::embeddings::SharedEmbedder;
#[cfg(feature = "code-search")]
use crate::lance::{CodeRow, PendingBatch};
use crate::scan_retrieval::{RetrievalError, VectorStore};
#[cfg(feature = "code-search")]
use crate::scanner_code::PendingCodeBatch;
use crate::store::Store;

/// The scan's vector store: the repository group's code-chunk LanceDB store a [`Store`] keeps open.
pub(crate) struct LanceVectors;

/// This build's retrieval stack, for the retrieval layer's own tests, which may not reach the engine
/// layer's binding (`crate::retrieval_wiring::backend`, which binds the same implementations).
#[cfg(test)]
pub(crate) fn test_retrieval() -> crate::scan_retrieval::Retrieval {
    crate::scan_retrieval::Retrieval {
        #[cfg(feature = "code-search")]
        embedder: &crate::embeddings::ScanEmbedder,
        #[cfg(feature = "code-search")]
        model_key: |preset| crate::embeddings::model_key(preset).into_owned(),
        vectors: &LanceVectors,
        #[cfg(feature = "documents")]
        documents: &crate::scanner_docs_lane::DocumentLane,
    }
}

impl VectorStore for LanceVectors {
    fn open(&self, store: &mut Store, config: &Config) -> Result<(), RetrievalError> {
        let preset = &config.documents.embedding_preset;
        SharedEmbedder::load(preset, 0, config.resources.embed_batch_size)
            .and_then(|embedder| store.lance_or_open(embedder.dim(), preset).map(|_| ()))
            .map_err(|error| RetrievalError(format!("{error:#}")))
    }

    fn holds_store(&self, store: &Store, id: &str) -> bool {
        std::fs::read_dir(crate::lance::code_stores_dir(&store.root))
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| crate::lance::store_id(&entry.path()).as_deref() == Some(id))
    }

    fn open_store_id(&self, store: &Store) -> Option<String> {
        store
            .lance
            .as_ref()
            .and_then(|lance| crate::lance::store_id(lance.dir()))
    }

    fn close(&self, store: &mut Store) {
        store.lance = None;
    }

    fn maintain(&self, store: &Store) {
        if let Some(lance) = store.lance.as_ref()
            && let Err(error) = lance.maintain(crate::lance::SCAN_RETENTION)
        {
            tracing::warn!(?error, "lance post-scan maintenance failed; old table versions kept");
        }
    }

    #[cfg(feature = "code-search")]
    fn write_code_rows(&self, store: &mut Store, batches: Vec<PendingCodeBatch>, model: &str) -> usize {
        flush_code_batches(store, batches, model)
    }

    #[cfg(feature = "code-search")]
    fn delete_code_rows(&self, store: &mut Store, config: &Config, paths: &[String]) -> bool {
        delete_code_rows(store, config, paths)
    }
}

/// Assemble the LanceDB rows for a file's chunks + embeddings (parallel arrays), **consuming** both
/// so the chunk text and embedding vectors move into the rows instead of being cloned. Called at
/// flush time with the owned sidecar blob; `content_hash` is stamped directly (no second pass needed).
#[cfg(feature = "code-search")]
fn build_rows_owned(
    content_hash: &str,
    chunks: Vec<crate::chunk::CodeChunk>,
    embeddings: Vec<Vec<f32>>,
) -> Vec<CodeRow> {
    chunks
        .into_iter()
        .zip(embeddings)
        .map(|(c, emb)| CodeRow {
            content_hash: content_hash.to_string(),
            chunk_id: c.chunk_id,
            symbol: c.symbol.unwrap_or_default(),
            kind: c.kind.unwrap_or_default(),
            lang: c.lang,
            line_start: c.line_start,
            line_end: c.line_end,
            byte_start: c.byte_start,
            byte_end: c.byte_end,
            text: c.text,
            embedding: emb,
        })
        .collect()
}

/// The contents whose `code_chunks` rows go, given that the files at `paths` left the scan set or
/// stopped being embed-eligible: every content the store holds rows for that no view of the group
/// holds (a removed file's), and the contents of the `paths` still in this view's file map that no
/// other file of this view and no other view holds. Another file of the content, here or in another
/// view, still searches by the rows.
///
/// `None` when the group's liveness cannot be established: nothing may be deleted then.
#[cfg(feature = "code-search")]
fn contents_to_drop(store: &Store, lance: &crate::lance::LanceStore, paths: &[String]) -> Option<Vec<String>> {
    let liveness = match store.group_content_liveness() {
        Ok(liveness) => liveness,
        Err(error) => {
            tracing::warn!(
                ?error,
                "code-chunk purge: the group's liveness is unknown; no rows deleted"
            );
            return None;
        }
    };
    let present = match lance.code_content_hashes() {
        Ok(present) => present,
        Err(error) => {
            tracing::warn!(
                ?error,
                "code-chunk purge: listing the stored contents failed; no rows deleted"
            );
            return None;
        }
    };
    let leaving: ahash::AHashSet<&str> = paths.iter().map(String::as_str).collect();
    let (going, staying): (Vec<_>, Vec<_>) = store
        .index
        .files
        .iter()
        .map(|(rel, entry)| (rel.to_str_lossy().into_owned(), entry.hash_hex.as_str()))
        .partition(|(rel, _)| leaving.contains(rel.as_str()));
    let staying: ahash::AHashSet<&str> = staying.into_iter().map(|(_, hash)| hash).collect();
    let mut doomed: ahash::AHashSet<String> = present.iter().filter(|hash| !liveness.holds(hash)).cloned().collect();
    doomed.extend(
        going
            .into_iter()
            .map(|(_, hash)| hash)
            .filter(|hash| present.contains(*hash) && !staying.contains(hash) && !liveness.siblings_hold(hash))
            .map(str::to_string),
    );
    let mut doomed: Vec<String> = doomed.into_iter().collect();
    doomed.sort_unstable();
    Some(doomed)
}

/// Delete the `code_chunks` rows left by `paths`, whether they left the scan set or stopped being
/// embed-eligible (see [`contents_to_drop`]). Best-effort: never opens (i.e. never *creates*) the
/// vector store when it does not already exist, and logs — never propagates — a delete failure.
///
/// Returns `false` only when rows that exist could not be deleted (unknown preset, store open,
/// liveness or delete error); "nothing to delete" and "no vector store" are both `true`.
#[cfg(feature = "code-search")]
fn delete_code_rows(store: &mut Store, config: &Config, paths: &[String]) -> bool {
    if paths.is_empty() {
        return true;
    }
    if store.lance.is_none() && !store.code_stores_exist() {
        return true;
    }
    let model = &config.documents.embedding_preset;
    let dim = match SharedEmbedder::load(model, 0, config.resources.embed_batch_size) {
        Ok(embedder) => embedder.dim(),
        Err(error) => {
            tracing::warn!(?error, preset = %model, "code-chunk stale purge: unknown embedding preset; skipping");
            return false;
        }
    };
    let lance = match store.lance_or_open(dim, model) {
        Ok(lance) => lance.clone(),
        Err(error) => {
            tracing::warn!(?error, "code-chunk stale purge: open LanceStore failed; skipping");
            return false;
        }
    };
    let gate = crate::lance::sweep_gate(lance.dir());
    let _exclusive = gate.write();
    let Some(contents) = contents_to_drop(store, &lance, paths) else {
        return false;
    };
    if let Err(error) = lance.delete_code_chunks_for_content(&contents) {
        tracing::warn!(
            contents = contents.len(),
            ?error,
            "code-chunk purge failed; search_code may return a removed or no-longer-embedded path"
        );
        return false;
    }
    true
}

/// Stream every pending code batch into the `code_chunks` LanceDB table, one file at a time. Opens
/// the store lazily — if no batch carries vectors (embeddings off) no LanceDB connection is made.
///
/// For each embedded file this re-reads the file's already-persisted `.chunk.msgpack` sidecar,
/// rebuilds its rows, writes them, and drops them before the next file — so peak embed-write RAM is
/// one file's rows plus one Arrow batch, independent of corpus size. (The sidecar is guaranteed on
/// disk here: every `embedding_dim > 0` batch either wrote it in `chunk_and_embed` or reused an
/// existing one, both before this post-barrier lane runs.)
///
/// The rows are keyed by content: files of identical content, in this view or another, are one set
/// of rows, written once per call and replaced, not added to, by a later write of the same content.
///
/// Errors are logged and skipped per file so one malformed embedding never aborts the scan. Returns
/// the number of files whose rows are written.
#[cfg(feature = "code-search")]
fn flush_code_batches(store: &mut Store, batches: Vec<PendingCodeBatch>, embedding_model: &str) -> usize {
    let Some(dim) = batches.iter().find(|b| b.embedding_dim > 0).map(|b| b.embedding_dim) else {
        return 0;
    };
    match SharedEmbedder::load(
        embedding_model,
        0,
        crate::config::ResourcesConfig::default().embed_batch_size,
    ) {
        Ok(embedder) if embedder.dim() != dim => {
            tracing::error!(
                preset = %embedding_model,
                expected = embedder.dim(),
                actual = dim,
                "preset/runtime dim mismatch — refusing to write code_chunks batch"
            );
            return 0;
        }
        Ok(_) => {}
        Err(error) => {
            tracing::error!(?error, preset = %embedding_model, "unknown embedding preset — refusing to write code_chunks batch");
            return 0;
        }
    }

    let lance = match store.lance_or_open(dim, embedding_model) {
        Ok(s) => s.clone(),
        Err(error) => {
            tracing::error!(?error, "open LanceStore for code_chunks batch failed");
            return 0;
        }
    };

    // Shared while a batch is written, so a sweep of the store never runs between its liveness read
    // and its deletes ([`crate::lance::SweepGate`]).
    let gate = crate::lance::sweep_gate(lance.dir());
    let mut inserted = 0usize;
    let mut pending: PendingBatch<CodeRow> = PendingBatch::default();
    // The paths each queued content stands for: a content queued by several files is written once,
    // and settles all of their debts.
    let mut owners: ahash::AHashMap<String, Vec<String>> = ahash::AHashMap::new();
    // Paths this call leaves owing nothing: rows written, or no rows to write. A path whose sidecar
    // could not be read or whose write failed stays owed for the next scan.
    let mut settled: Vec<String> = Vec::new();
    let write = |pending: &mut PendingBatch<CodeRow>,
                 owners: &mut ahash::AHashMap<String, Vec<String>>,
                 inserted: &mut usize,
                 settled: &mut Vec<String>| {
        let items = pending.take();
        let paths: Vec<String> = items
            .iter()
            .filter_map(|(hash, _)| owners.remove(hash))
            .flatten()
            .collect();
        let _shared = gate.read();
        match lance.replace_code_chunks_many(items) {
            Ok(()) => {
                *inserted += paths.len();
                settled.extend(paths);
            }
            Err(error) => {
                tracing::warn!(
                    files = paths.len(),
                    ?error,
                    "lance replace_code_chunks_many failed; the files stay queued for the next scan"
                );
            }
        }
    };
    for batch in batches {
        if batch.embedding_dim == 0 {
            continue;
        }
        if let Some(paths) = owners.get_mut(&batch.blob_hash) {
            paths.push(batch.rel_path);
            continue;
        }
        let blob = match store.read_chunks_by_hex(&batch.blob_hash) {
            Ok(Some(blob)) if blob.embedding_dim == dim && blob.embeddings.len() == blob.chunks.len() => blob,
            Ok(Some(_)) => {
                settled.push(batch.rel_path);
                continue;
            }
            // No sidecar, no rows to build: the file's next chunking queues it again.
            Ok(None) => {
                tracing::warn!(rel = %batch.rel_path, "chunk sidecar missing at flush; no vector rows to write");
                settled.push(batch.rel_path);
                continue;
            }
            Err(error) => {
                tracing::warn!(rel = %batch.rel_path, ?error, "re-read chunk sidecar failed; skipping vector rows");
                continue;
            }
        };
        if blob.chunks.is_empty() {
            settled.push(batch.rel_path);
            continue;
        }
        let rows = build_rows_owned(&batch.blob_hash, blob.chunks, blob.embeddings);
        pending.push(&batch.blob_hash, rows);
        owners.insert(batch.blob_hash, vec![batch.rel_path]);
        if pending.is_full() {
            write(&mut pending, &mut owners, &mut inserted, &mut settled);
        }
    }
    if !pending.is_empty() {
        write(&mut pending, &mut owners, &mut inserted, &mut settled);
    }
    store.pending_vectors.settle_code(&settled);
    inserted
}

#[cfg(all(test, feature = "code-search"))]
mod tests {
    use crate::lance::CodeRow;
    use crate::scanner_code::purge_unembedded_code;
    use crate::scanner_filter::Filters;
    use crate::store::Store;

    /// The content of `path` unless `content` names another: files given the same `content` are
    /// copies of one another.
    fn hash_of(path: &str, content: Option<&str>) -> String {
        format!("content-{}", content.unwrap_or(path))
    }

    /// Index each `(path, content)` and give its content one chunk row in the group's store.
    fn seeded_code_lance(store: &mut Store, files: &[(&str, Option<&str>)]) -> crate::lance::LanceStore {
        let lance = store.lance_or_open(768, "balanced").expect("open lance").clone();
        for (path, content) in files {
            let hash = hash_of(path, *content);
            let row = CodeRow {
                content_hash: hash.clone(),
                chunk_id: format!("{hash}:0"),
                symbol: "sym".to_string(),
                kind: "function".to_string(),
                lang: "rust".to_string(),
                line_start: 1,
                line_end: 1,
                byte_start: 0,
                byte_end: 4,
                text: "body".to_string(),
                embedding: vec![0.25; 768],
            };
            lance
                .replace_code_chunks_many(vec![(hash.clone(), vec![row])])
                .expect("seed row");
            store.upsert(
                *path,
                crate::store::FileEntry {
                    hash_hex: hash,
                    language: "rust".to_string(),
                    size_bytes: 1,
                    mtime: 0,
                    extract_epoch: crate::extract::EXTRACT_EPOCH,
                    content_key: None,
                    lines: 0,
                },
            );
        }
        lance
    }

    /// The contents with rows, as the paths `files` gave them.
    fn code_rows(lance: &crate::lance::LanceStore, files: &[(&str, Option<&str>)]) -> Vec<String> {
        let held = lance.code_content_hashes().expect("contents");
        let mut paths: Vec<String> = files
            .iter()
            .filter(|(path, content)| held.contains(&hash_of(path, *content)))
            .map(|(path, _)| (*path).to_string())
            .collect();
        paths.sort();
        paths
    }

    const FILES: &[(&str, Option<&str>)] = &[("src/a.rs", None), ("src/generated/b.rs", None), ("lib/c.rs", None)];

    #[test]
    fn embed_policy_purge_deletes_vectors_of_newly_ineligible_files_only() {
        crate::store::init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(tmp.path(), crate::store::VIEW_WORKING).unwrap();
        let lance = seeded_code_lance(&mut store, FILES);
        assert_eq!(code_rows(&lance, FILES).len(), 3);

        let mut config = crate::config::default_for_root(std::path::Path::new("."));
        config.code_search.embed = true;
        config.code_search.embed_exclude = vec!["**/generated/**".to_string()];
        let filters = Filters::build(&config, Vec::new(), crate::scanner_vectors::test_retrieval()).unwrap();
        purge_unembedded_code(&mut store, &config, &filters);
        assert_eq!(
            code_rows(&lance, FILES),
            vec!["lib/c.rs".to_string(), "src/a.rs".to_string()]
        );

        config.code_search.embed_include = vec!["src/**".to_string()];
        let filters = Filters::build(&config, Vec::new(), crate::scanner_vectors::test_retrieval()).unwrap();
        purge_unembedded_code(&mut store, &config, &filters);
        assert_eq!(
            code_rows(&lance, FILES),
            vec!["src/a.rs".to_string()],
            "embed_include narrows retroactively"
        );

        config.code_search.embed = false;
        let filters = Filters::build(&config, Vec::new(), crate::scanner_vectors::test_retrieval()).unwrap();
        purge_unembedded_code(&mut store, &config, &filters);
        assert!(
            code_rows(&lance, FILES).is_empty(),
            "embed = false removes every vector row"
        );
        assert!(store.lookup("src/a.rs").is_some(), "files stay indexed");
    }

    /// The rows are the content's, not the path's: a copy that is still eligible keeps them.
    #[test]
    fn embed_policy_purge_keeps_the_rows_of_content_an_eligible_copy_holds() {
        crate::store::init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(tmp.path(), crate::store::VIEW_WORKING).unwrap();
        let files: &[(&str, Option<&str>)] = &[
            ("src/a.rs", Some("same")),
            ("src/generated/a_copy.rs", Some("same")),
            ("src/generated/only.rs", None),
        ];
        let lance = seeded_code_lance(&mut store, files);

        let mut config = crate::config::default_for_root(std::path::Path::new("."));
        config.code_search.embed = true;
        config.code_search.embed_exclude = vec!["**/generated/**".to_string()];
        let filters = Filters::build(&config, Vec::new(), crate::scanner_vectors::test_retrieval()).unwrap();
        assert!(purge_unembedded_code(&mut store, &config, &filters));

        assert_eq!(
            code_rows(&lance, files),
            vec!["src/a.rs".to_string(), "src/generated/a_copy.rs".to_string()],
            "the copy's rows stay for src/a.rs; the content only generated/ holds goes"
        );
    }

    /// A removed file's rows go with the last file of its content, and no earlier.
    #[test]
    fn removing_a_file_deletes_the_rows_of_content_no_file_holds_any_more() {
        crate::store::init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(tmp.path(), crate::store::VIEW_WORKING).unwrap();
        let files: &[(&str, Option<&str>)] = &[
            ("src/a.rs", Some("same")),
            ("src/a_copy.rs", Some("same")),
            ("src/b.rs", None),
        ];
        let lance = seeded_code_lance(&mut store, files);
        let config = crate::config::default_for_root(std::path::Path::new("."));

        store.remove("src/a.rs");
        assert!(super::delete_code_rows(&mut store, &config, &["src/a.rs".to_string()]));
        assert_eq!(code_rows(&lance, files).len(), 3, "the copy still holds the content");

        store.remove("src/a_copy.rs");
        store.remove("src/b.rs");
        assert!(super::delete_code_rows(
            &mut store,
            &config,
            &["src/a_copy.rs".to_string(), "src/b.rs".to_string()]
        ));
        assert!(code_rows(&lance, files).is_empty());
    }

    #[test]
    fn a_purge_that_cannot_delete_reports_failure_and_leaves_the_rows() {
        crate::store::init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(tmp.path(), crate::store::VIEW_WORKING).unwrap();
        let files: &[(&str, Option<&str>)] = &[("src/a.rs", None)];
        let lance = seeded_code_lance(&mut store, files);

        let mut config = crate::config::default_for_root(std::path::Path::new("."));
        config.code_search.embed = false;
        let filters = Filters::build(&config, Vec::new(), crate::scanner_vectors::test_retrieval()).unwrap();

        config.documents.embedding_preset = "no-such-preset".to_string();
        assert!(
            !purge_unembedded_code(&mut store, &config, &filters),
            "an unknown preset means the delete never ran: not applied"
        );
        assert_eq!(
            code_rows(&lance, files),
            vec!["src/a.rs".to_string()],
            "nothing was deleted"
        );

        config.documents.embedding_preset = "balanced".to_string();
        assert!(purge_unembedded_code(&mut store, &config, &filters));
        assert!(code_rows(&lance, files).is_empty(), "the retry removes the row");
    }
}
