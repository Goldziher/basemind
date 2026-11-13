//! Experimental shared tables. Standalone scanner and query routing do not use these yet.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use arrow_array::{Array, RecordBatch, RecordBatchIterator, StringArray, UInt32Array};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use super::schema;
use super::shared::{ContentHit, LivePaths, PathHit, search_live, shared_directory};

const CHUNK_KEY: &[&str] = &["scope", "content_hash", "chunk_idx"];
#[cfg(feature = "documents")]
const LINK_KEY: &[&str] = &["scope", "content_hash", "chunk_idx", "mention_kind", "mention_value"];

#[derive(Clone, Copy)]
pub enum SharedLane {
    Documents,
    #[cfg(feature = "code-search")]
    Code,
    #[cfg(feature = "documents")]
    DocLinks,
}

impl SharedLane {
    fn name(self) -> &'static str {
        match self {
            Self::Documents => schema::DOCUMENTS_TABLE,
            #[cfg(feature = "code-search")]
            Self::Code => schema::CODE_CHUNKS_TABLE,
            #[cfg(feature = "documents")]
            Self::DocLinks => schema::DOC_LINKS_TABLE,
        }
    }

    fn keys(self) -> &'static [&'static str] {
        match self {
            #[cfg(feature = "documents")]
            Self::DocLinks => LINK_KEY,
            _ => CHUNK_KEY,
        }
    }

    fn schema(self, dim: u16) -> arrow_schema::SchemaRef {
        match self {
            Self::Documents => schema::shared_documents_schema(dim),
            #[cfg(feature = "code-search")]
            Self::Code => schema::shared_code_chunks_schema(dim),
            #[cfg(feature = "documents")]
            Self::DocLinks => schema::shared_doc_links_schema(),
        }
    }
}

pub struct SharedStore {
    connection: lancedb::Connection,
    runtime: tokio::runtime::Runtime,
    group: PathBuf,
    dim: u16,
}

impl SharedStore {
    pub fn open(group: &Path, model: &str, dim: u16) -> Result<Self> {
        ensure!(dim != 0, "embedding dimension must be positive");
        fs::create_dir_all(group)?;
        let _lock = writer_lock(group)?;
        let directory = shared_directory(group, model, dim);
        fs::create_dir_all(&directory)?;
        let uri = directory.to_str().context("shared Lance path must be UTF-8")?;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let connection = runtime.block_on(async {
            let connection = lancedb::connect(uri).execute().await?;
            ensure_lane(&connection, SharedLane::Documents, dim).await?;
            #[cfg(feature = "code-search")]
            ensure_lane(&connection, SharedLane::Code, dim).await?;
            #[cfg(feature = "documents")]
            ensure_lane(&connection, SharedLane::DocLinks, dim).await?;
            anyhow::Ok(connection)
        })?;
        Ok(Self {
            connection,
            runtime,
            group: group.to_path_buf(),
            dim,
        })
    }

    /// Insert an immutable batch atomically. A retry cannot replace existing rows.
    pub fn insert(&self, lane: SharedLane, batch: RecordBatch) -> Result<()> {
        ensure!(
            batch.schema() == lane.schema(self.dim),
            "unexpected shared Lance batch schema"
        );
        if batch.num_rows() == 0 {
            return Ok(());
        }
        validate_unique_keys(lane, &batch)?;
        let _lock = writer_lock(&self.group)?;
        self.runtime.block_on(async {
            let table = self.connection.open_table(lane.name()).execute().await?;
            let mut merge = table.merge_insert(lane.keys());
            merge.when_not_matched_insert_all();
            let schema = batch.schema();
            let rows = RecordBatchIterator::new([Ok(batch)], schema);
            merge.execute(Box::new(rows)).await?;
            anyhow::Ok(())
        })
    }

    /// Scope and MIME filters run before ranking. The manifest determines which paths are live.
    pub fn search_documents(
        &self,
        scope: &str,
        query: Vec<f32>,
        mime: Option<&str>,
        paths: &LivePaths<'_>,
        limit: usize,
    ) -> Result<Vec<super::DocumentHit>> {
        let mut filter = scope_filter(scope);
        if let Some(mime) = mime {
            filter.push_str(&format!(" AND mime_type = '{}'", super::escape_sql_literal(mime)));
        }
        let hits = self.search_lane(SharedLane::Documents, query, &filter, paths, limit, decode_document)?;
        Ok(hits
            .into_iter()
            .map(|hit| super::DocumentHit {
                path: hit.path.into(),
                ..hit.value
            })
            .collect())
    }

    #[cfg(feature = "code-search")]
    pub fn search_code_chunks(
        &self,
        scope: &str,
        query: Vec<f32>,
        paths: &LivePaths<'_>,
        limit: usize,
    ) -> Result<Vec<super::CodeChunkHit>> {
        let hits = self.search_lane(SharedLane::Code, query, &scope_filter(scope), paths, limit, decode_code)?;
        Ok(hits
            .into_iter()
            .map(|hit| super::CodeChunkHit {
                path: hit.path.into(),
                ..hit.value
            })
            .collect())
    }

    fn search_lane<'a, T: Clone>(
        &self,
        lane: SharedLane,
        query: Vec<f32>,
        filter: &str,
        paths: &LivePaths<'a>,
        limit: usize,
        decode: impl Fn(&RecordBatch, &mut Vec<ContentHit<T>>) -> Result<()>,
    ) -> Result<Vec<PathHit<'a, T>>> {
        ensure!(query.len() == usize::from(self.dim), "wrong query dimension");
        ensure!(
            lane.schema(self.dim).field_with_name("embedding").is_ok(),
            "lane has no vector column"
        );
        let table = self
            .runtime
            .block_on(self.connection.open_table(lane.name()).execute())?;
        search_live(paths, limit, |count| {
            self.runtime.block_on(async {
                let mut stream = table
                    .vector_search(query.clone())?
                    .only_if(filter)
                    .limit(count)
                    .execute()
                    .await?;
                let mut hits = Vec::new();
                while let Some(batch) = stream.try_next().await? {
                    decode(&batch, &mut hits)?;
                }
                anyhow::Ok(hits)
            })
        })
    }

    /// Restore link rows for the effective document manifest and the selected scope.
    #[cfg(feature = "documents")]
    pub fn doc_links(&self, scope: &str, paths: &LivePaths<'_>) -> Result<Vec<super::DocLinkRow>> {
        self.runtime.block_on(async {
            let table = self.connection.open_table(schema::DOC_LINKS_TABLE).execute().await?;
            let predicate = scope_filter(scope);
            let mut stream = table.query().only_if(predicate).execute().await?;
            let mut rows = Vec::new();
            while let Some(batch) = stream.try_next().await? {
                project_links(scope, paths, &batch, &mut rows)?;
            }
            rows.sort_by(|a, b| {
                (&a.doc_path, a.chunk_idx, &a.mention_kind, &a.mention_value).cmp(&(
                    &b.doc_path,
                    b.chunk_idx,
                    &b.mention_kind,
                    &b.mention_value,
                ))
            });
            Ok(rows)
        })
    }
}

fn scope_filter(scope: &str) -> String {
    format!("scope = '{}'", super::escape_sql_literal(scope))
}

#[cfg(feature = "code-search")]
fn decode_code(batch: &RecordBatch, out: &mut Vec<ContentHit<super::CodeChunkHit>>) -> Result<()> {
    let mut pointers = Vec::new();
    super::decode_code_chunk_hits_with_path_column(batch, "content_hash", &mut pointers)?;
    let ordinals = column::<UInt32Array>(batch, "chunk_idx")?;
    out.extend(pointers.into_iter().enumerate().map(|(index, mut value)| ContentHit {
        hash: std::mem::take(&mut value.path),
        chunk_idx: ordinals.value(index),
        distance: value.distance,
        value,
    }));
    Ok(())
}

#[cfg(feature = "documents")]
fn project_links(
    scope: &str,
    paths: &LivePaths<'_>,
    batch: &RecordBatch,
    rows: &mut Vec<super::DocLinkRow>,
) -> Result<()> {
    let hashes = column::<StringArray>(batch, "content_hash")?;
    let ordinals = column::<UInt32Array>(batch, "chunk_idx")?;
    let kinds = column::<StringArray>(batch, "mention_kind")?;
    let values = column::<StringArray>(batch, "mention_value")?;
    for index in 0..batch.num_rows() {
        rows.extend(paths.paths_for(hashes.value(index)).map(|path| super::DocLinkRow {
            scope: scope.into(),
            doc_path: path.into(),
            chunk_idx: ordinals.value(index),
            mention_kind: kinds.value(index).into(),
            mention_value: values.value(index).into(),
        }));
    }
    Ok(())
}

fn validate_unique_keys(lane: SharedLane, batch: &RecordBatch) -> Result<()> {
    let mut keys = std::collections::HashSet::with_capacity(batch.num_rows());
    let columns = lane
        .keys()
        .iter()
        .map(|key| batch.column_by_name(key).context("missing key"))
        .collect::<Result<Vec<_>>>()?;
    for index in 0..batch.num_rows() {
        let key = columns
            .iter()
            .map(|column| {
                ensure!(!column.is_null(index), "null shared key");
                if let Some(strings) = column.as_any().downcast_ref::<StringArray>() {
                    Ok(strings.value(index).to_owned())
                } else {
                    Ok(column
                        .as_any()
                        .downcast_ref::<UInt32Array>()
                        .context("invalid ordinal key")?
                        .value(index)
                        .to_string())
                }
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(keys.insert(key), "duplicate key in shared Lance batch");
    }
    Ok(())
}

fn decode_document(batch: &RecordBatch, out: &mut Vec<ContentHit<super::DocumentHit>>) -> Result<()> {
    let mut documents = Vec::new();
    super::decode_document_hits_with_path_column(batch, "content_hash", &mut documents)?;
    out.extend(documents.into_iter().map(|mut value| ContentHit {
        hash: std::mem::take(&mut value.path),
        chunk_idx: value.chunk_idx,
        distance: value.distance,
        value,
    }));
    Ok(())
}

#[cfg(any(feature = "documents", feature = "code-search"))]
fn column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a T> {
    batch
        .column_by_name(name)
        .and_then(|value| value.as_any().downcast_ref())
        .with_context(|| format!("missing or invalid {name}"))
}

async fn ensure_lane(connection: &lancedb::Connection, lane: SharedLane, dim: u16) -> Result<()> {
    let schema = lane.schema(dim);
    super::ensure_table(connection, lane.name(), schema.clone()).await?;
    let table = connection.open_table(lane.name()).execute().await?;
    ensure!(
        table.schema().await? == schema,
        "incompatible shared Lance table {}",
        lane.name()
    );
    Ok(())
}

fn writer_lock(group: &Path) -> Result<std::fs::File> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(group.join("lance.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lance::{DocumentRow, build_documents_batch};

    fn document(scope: &str, hash: &str, text: &str, vector: Vec<f32>) -> RecordBatch {
        let batch = build_documents_batch(
            2,
            &[DocumentRow {
                scope: scope.into(),
                path: hash.into(),
                chunk_idx: 0,
                mime_type: "text/plain".into(),
                text: text.into(),
                byte_start: 0,
                byte_end: 1,
                embedding: vector,
            }],
        )
        .unwrap();
        RecordBatch::try_new(schema::shared_documents_schema(2), batch.columns().to_vec()).unwrap()
    }

    #[cfg(feature = "code-search")]
    #[test]
    fn shared_code_preserves_pointer_fields_under_the_effective_path() {
        use crate::lance::{CodeRow, build_code_chunks_batch};

        let group = tempfile::tempdir().unwrap();
        let store = SharedStore::open(group.path(), "test-model", 2).unwrap();
        let entry = crate::store::FileEntry {
            hash_hex: "blob".into(),
            language: "rust".into(),
            size_bytes: 10,
            mtime: 0,
            extract_epoch: 1,
        };
        let mut other_language = entry.clone();
        other_language.language = "typescript".into();
        let mut row = CodeRow {
            scope: "project's scope".into(),
            path: super::super::shared::code_content_key(&entry),
            chunk_id: "blob:1".into(),
            symbol: "run".into(),
            kind: "function".into(),
            lang: "rust".into(),
            line_start: 10,
            line_end: 12,
            byte_start: 20,
            byte_end: 48,
            text: "fn run() {}".into(),
            embedding: vec![1.0, 0.0],
        };
        let batch = build_code_chunks_batch(2, &[row.clone()]).unwrap();
        let mut columns = batch.columns().to_vec();
        columns.push(std::sync::Arc::new(UInt32Array::from(vec![1])));
        let shared = RecordBatch::try_new(schema::shared_code_chunks_schema(2), columns).unwrap();
        store.insert(SharedLane::Code, shared).unwrap();
        row.scope = "unrelated".into();
        row.embedding = vec![0.0, 0.0];
        let batch = build_code_chunks_batch(2, &[row]).unwrap();
        let mut columns = batch.columns().to_vec();
        columns.push(std::sync::Arc::new(UInt32Array::from(vec![1])));
        let shared = RecordBatch::try_new(schema::shared_code_chunks_schema(2), columns).unwrap();
        store.insert(SharedLane::Code, shared).unwrap();

        let live = LivePaths::from_code_entries([("renamed.rs", &entry), ("unrelated.ts", &other_language)]);
        let hits = store
            .search_code_chunks("project's scope", vec![0.0, 0.0], &live, 2)
            .unwrap();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!((&*hit.path, &*hit.chunk_id), ("renamed.rs", "blob:1"));
        assert_eq!((&*hit.symbol, &*hit.kind, &*hit.lang), ("run", "function", "rust"));
        assert_eq!(
            (hit.line_start, hit.line_end, hit.byte_start, hit.byte_end),
            (10, 12, 20, 48)
        );
        assert_eq!(hit.distance, 1.0);
    }

    #[cfg(feature = "documents")]
    #[test]
    fn shared_links_keep_distinct_mentions_and_project_renames() {
        use crate::lance::{DocLinkRow, doc_links::build_doc_links_batch};

        let group = tempfile::tempdir().unwrap();
        let store = SharedStore::open(group.path(), "test-model", 2).unwrap();
        let rows = ["Beta", "Alpha"].map(|value| DocLinkRow {
            scope: "repo".into(),
            doc_path: "live".into(),
            chunk_idx: 1,
            mention_kind: "name".into(),
            mention_value: value.into(),
        });
        let batch = build_doc_links_batch(&rows).unwrap();
        let shared = RecordBatch::try_new(schema::shared_doc_links_schema(), batch.columns().to_vec()).unwrap();
        store.insert(SharedLane::DocLinks, shared.clone()).unwrap();
        store.insert(SharedLane::DocLinks, shared).unwrap();
        let paths = LivePaths::from_iter([("live", "renamed.md")]);
        let links = store.doc_links("repo", &paths).unwrap();
        assert_eq!(
            links
                .iter()
                .map(|row| (row.doc_path.as_str(), row.mention_value.as_str()))
                .collect::<Vec<_>>(),
            [("renamed.md", "Alpha"), ("renamed.md", "Beta")]
        );
    }

    #[test]
    fn shared_documents_reopen_without_replacing_rows_or_exposing_stale_paths() {
        let group = tempfile::tempdir().unwrap();
        let lane = SharedLane::Documents;
        let store = SharedStore::open(group.path(), "test-model", 2).unwrap();
        store
            .insert(lane, document("scope", "old", "stale", vec![0.0, 0.0]))
            .unwrap();
        store
            .insert(lane, document("scope", "live", "original", vec![1.0, 0.0]))
            .unwrap();
        store
            .insert(lane, document("scope", "live", "retry", vec![0.0, 0.0]))
            .unwrap();
        store
            .insert(lane, document("other", "live", "foreign", vec![0.0, 0.0]))
            .unwrap();

        drop(store);
        let store = SharedStore::open(group.path(), "test-model", 2).unwrap();
        let live = LivePaths::from_iter([("live", "b.txt"), ("live", "a.txt")]);
        let hits = store
            .search_documents("scope", vec![0.0, 0.0], Some("text/plain"), &live, 3)
            .unwrap();
        assert_eq!(
            hits.iter()
                .map(|hit| (hit.path.as_str(), hit.text.as_str()))
                .collect::<Vec<_>>(),
            [("a.txt", "original"), ("b.txt", "original")]
        );
        assert_eq!(
            (hits[0].byte_start, hits[0].byte_end, hits[0].mime_type.as_str()),
            (0, 1, "text/plain")
        );
        assert!(
            store
                .search_documents("scope", vec![0.0, 0.0], Some("text/html"), &live, 3)
                .unwrap()
                .is_empty()
        );
    }
}
