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
            let rows = RecordBatchIterator::new([Ok(batch.clone())], batch.schema());
            merge.execute(Box::new(rows)).await?;
            anyhow::Ok(())
        })
    }

    /// Scope and MIME filters run before ranking. The manifest determines which paths are live.
    pub fn search<'a>(
        &self,
        lane: SharedLane,
        scope: &str,
        query: Vec<f32>,
        mime: Option<&str>,
        paths: &LivePaths<'a>,
        limit: usize,
    ) -> Result<Vec<PathHit<'a, String>>> {
        ensure!(query.len() == usize::from(self.dim), "wrong query dimension");
        ensure!(
            lane.schema(self.dim).field_with_name("embedding").is_ok(),
            "lane has no vector column"
        );
        ensure!(
            mime.is_none() || matches!(lane, SharedLane::Documents),
            "MIME filter requires documents"
        );
        let mut filter = format!("scope = '{}'", super::escape_sql_literal(scope));
        if let Some(mime) = mime {
            filter.push_str(&format!(" AND mime_type = '{}'", super::escape_sql_literal(mime)));
        }
        search_live(paths, limit, |count| {
            self.runtime.block_on(async {
                let table = self.connection.open_table(lane.name()).execute().await?;
                let mut stream = table
                    .vector_search(query.clone())?
                    .only_if(filter.as_str())
                    .limit(count)
                    .execute()
                    .await?;
                let mut hits = Vec::new();
                while let Some(batch) = stream.try_next().await? {
                    decode(&batch, &mut hits)?;
                }
                Ok(hits)
            })
        })
    }
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

fn decode(batch: &RecordBatch, out: &mut Vec<ContentHit<String>>) -> Result<()> {
    let hashes = column::<StringArray>(batch, "content_hash")?;
    let ordinals = column::<UInt32Array>(batch, "chunk_idx")?;
    let distances = column::<arrow_array::Float32Array>(batch, "_distance")?;
    let text = column::<StringArray>(batch, "text")?;
    for index in 0..batch.num_rows() {
        out.push(ContentHit {
            hash: hashes.value(index).into(),
            chunk_idx: ordinals.value(index),
            distance: distances.value(index),
            value: text.value(index).into(),
        });
    }
    Ok(())
}

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
            .search(lane, "scope", vec![0.0, 0.0], Some("text/plain"), &live, 3)
            .unwrap();
        assert_eq!(
            hits.iter()
                .map(|hit| (hit.path, hit.value.as_str()))
                .collect::<Vec<_>>(),
            [("a.txt", "original"), ("b.txt", "original")]
        );
        assert!(
            store
                .search(lane, "scope", vec![0.0, 0.0], Some("text/html"), &live, 3)
                .unwrap()
                .is_empty()
        );
    }
}
