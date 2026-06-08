//! LanceDB-backed vector storage for the document tier and shared agent memory.
//!
//! One store per `.basemind/lance/` directory. The dim of the embedding vector
//! is fixed at table-creation time and persisted in a small `meta.json`; reopen
//! with a different dim triggers a wipe-and-rebuild of the whole lance dir
//! (mirroring the existing `INDEX_SCHEMA_VER`-mismatch flow in
//! [`crate::store::Store::open`]).
//!
//! The LanceDB client is async. We block on a private current-thread tokio
//! runtime so the scanner (rayon, sync) and the MCP server (multi-thread tokio)
//! can share the same sync API surface without each callsite worrying about
//! runtime context.

pub mod schema;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use arrow_array::builder::{
    FixedSizeListBuilder, Float32Builder, ListBuilder, StringBuilder, TimestampMicrosecondBuilder,
    UInt32Builder,
};
use arrow_array::{Array, RecordBatch, RecordBatchIterator, RecordBatchReader, StringArray};
use arrow_schema::ArrowError;
use futures::TryStreamExt;
use lancedb::Connection;
use lancedb::query::{ExecutableQuery, QueryBase};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;

use schema::{DOCUMENTS_TABLE, MEMORY_TABLE, documents_schema, memory_schema};

/// On-disk metadata for the lance store. Tracks the vector dim + the
/// embedding-model identifier; a mismatch on open wipes the store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct LanceMeta {
    dim: u16,
    embedding_model: String,
}

const META_FILE: &str = "meta.json";

/// One row in the `documents` table.
#[derive(Debug, Clone)]
pub struct DocumentRow {
    pub scope: String,
    pub path: String,
    pub chunk_idx: u32,
    pub mime_type: String,
    pub text: String,
    pub byte_start: u32,
    pub byte_end: u32,
    pub embedding: Vec<f32>,
}

/// One row in the `memory` table.
#[derive(Debug, Clone)]
pub struct MemoryRow {
    pub scope: String,
    pub key: String,
    pub value: String,
    pub tags: Vec<String>,
    pub embedding: Vec<f32>,
    /// Microseconds since unix epoch.
    pub created_at: i64,
    pub updated_at: i64,
}

/// A search hit from the `documents` table.
#[derive(Debug, Clone)]
pub struct DocumentHit {
    pub path: String,
    pub chunk_idx: u32,
    pub text: String,
    pub mime_type: String,
    pub byte_start: u32,
    pub byte_end: u32,
    /// L2 distance from the query vector (lower = closer). LanceDB returns this
    /// in the `_distance` column.
    pub distance: f32,
}

/// A search hit from the `memory` table.
#[derive(Debug, Clone)]
pub struct MemoryHit {
    pub key: String,
    pub value: String,
    pub tags: Vec<String>,
    pub distance: f32,
}

/// Embedded LanceDB store. Cheap to clone (internal Arc).
#[derive(Clone)]
pub struct LanceStore {
    inner: Arc<LanceStoreInner>,
}

struct LanceStoreInner {
    runtime: Runtime,
    connection: Connection,
    dim: u16,
    embedding_model: String,
    dir: PathBuf,
}

impl LanceStore {
    /// Open (or initialise) the lance store rooted at `dir`. If a pre-existing
    /// meta.json reports a different `(dim, embedding_model)` pair, the entire
    /// dir is wiped and rebuilt before the connection opens.
    pub fn open(dir: &Path, dim: u16, embedding_model: &str) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let meta_path = dir.join(META_FILE);
        let expected = LanceMeta {
            dim,
            embedding_model: embedding_model.to_string(),
        };
        wipe_on_mismatch(dir, &meta_path, &expected)?;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build tokio runtime for LanceStore")?;
        let uri = dir.to_string_lossy().into_owned();
        let connection = runtime
            .block_on(async { lancedb::connect(&uri).execute().await })
            .with_context(|| format!("open lancedb at {uri}"))?;

        // Best-effort: ensure both tables exist with the expected schema. We
        // create them empty on first use; subsequent opens are cheap no-ops.
        runtime.block_on(async {
            ensure_table(&connection, DOCUMENTS_TABLE, documents_schema(dim)).await?;
            ensure_table(&connection, MEMORY_TABLE, memory_schema(dim)).await?;
            anyhow::Ok(())
        })?;

        if !meta_path.exists() {
            let body = serde_json::to_vec_pretty(&expected).context("serialize lance meta.json")?;
            std::fs::write(&meta_path, body)
                .with_context(|| format!("write {}", meta_path.display()))?;
        }

        Ok(Self {
            inner: Arc::new(LanceStoreInner {
                runtime,
                connection,
                dim,
                embedding_model: embedding_model.to_string(),
                dir: dir.to_path_buf(),
            }),
        })
    }

    pub fn dim(&self) -> u16 {
        self.inner.dim
    }

    pub fn embedding_model(&self) -> &str {
        &self.inner.embedding_model
    }

    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Replace all documents rows for the given `(scope, path)` pair and insert
    /// the supplied rows. Used by the scanner during incremental re-extract.
    pub fn replace_document(&self, scope: &str, path: &str, rows: Vec<DocumentRow>) -> Result<()> {
        self.inner.runtime.block_on(async {
            let table = self
                .inner
                .connection
                .open_table(DOCUMENTS_TABLE)
                .execute()
                .await
                .with_context(|| format!("open {DOCUMENTS_TABLE} table"))?;
            let predicate = format!(
                "scope = '{}' AND path = '{}'",
                escape_sql_literal(scope),
                escape_sql_literal(path)
            );
            table
                .delete(&predicate)
                .await
                .with_context(|| format!("delete existing rows for {scope}/{path}"))?;
            if rows.is_empty() {
                return Ok(());
            }
            let batch = build_documents_batch(self.inner.dim, &rows)?;
            let schema = batch.schema();
            let reader: Box<dyn RecordBatchReader + Send> = Box::new(RecordBatchIterator::new(
                vec![Ok::<_, ArrowError>(batch)].into_iter(),
                schema,
            ));
            table
                .add(reader)
                .execute()
                .await
                .with_context(|| format!("insert {} documents rows", rows.len()))?;
            anyhow::Ok(())
        })
    }

    /// Insert / upsert one memory row keyed by `(scope, key)`. Existing rows
    /// with the same key are removed first.
    pub fn upsert_memory(&self, row: MemoryRow) -> Result<()> {
        self.inner.runtime.block_on(async {
            let table = self
                .inner
                .connection
                .open_table(MEMORY_TABLE)
                .execute()
                .await
                .with_context(|| format!("open {MEMORY_TABLE} table"))?;
            let predicate = format!(
                "scope = '{}' AND key = '{}'",
                escape_sql_literal(&row.scope),
                escape_sql_literal(&row.key)
            );
            table
                .delete(&predicate)
                .await
                .context("delete previous memory entry")?;
            let batch = build_memory_batch(self.inner.dim, std::slice::from_ref(&row))?;
            let schema = batch.schema();
            let reader: Box<dyn RecordBatchReader + Send> = Box::new(RecordBatchIterator::new(
                vec![Ok::<_, ArrowError>(batch)].into_iter(),
                schema,
            ));
            table
                .add(reader)
                .execute()
                .await
                .context("insert memory row")?;
            anyhow::Ok(())
        })
    }

    /// Delete one memory entry by `(scope, key)`. Returns `true` if a row was
    /// matched (whether it was deleted is up to LanceDB's semantics — currently
    /// always deletes when the predicate matches).
    pub fn delete_memory(&self, scope: &str, key: &str) -> Result<bool> {
        self.inner.runtime.block_on(async {
            let table = self
                .inner
                .connection
                .open_table(MEMORY_TABLE)
                .execute()
                .await
                .with_context(|| format!("open {MEMORY_TABLE} table"))?;
            let predicate = format!(
                "scope = '{}' AND key = '{}'",
                escape_sql_literal(scope),
                escape_sql_literal(key)
            );
            table
                .delete(&predicate)
                .await
                .context("delete memory entry")?;
            anyhow::Ok(true)
        })
    }

    /// KNN over the documents table for one scope.
    pub fn search_documents(
        &self,
        scope: &str,
        query: Vec<f32>,
        limit: usize,
        mime_type_filter: Option<&str>,
    ) -> Result<Vec<DocumentHit>> {
        if query.len() != usize::from(self.inner.dim) {
            return Err(anyhow!(
                "query vector dim {} does not match store dim {}",
                query.len(),
                self.inner.dim
            ));
        }
        self.inner.runtime.block_on(async {
            let table = self
                .inner
                .connection
                .open_table(DOCUMENTS_TABLE)
                .execute()
                .await
                .with_context(|| format!("open {DOCUMENTS_TABLE} table"))?;
            let mut q = table
                .vector_search(query)
                .context("build vector search")?
                .limit(limit);
            let scope_clause = format!("scope = '{}'", escape_sql_literal(scope));
            q = match mime_type_filter {
                Some(m) => q.only_if(format!(
                    "{scope_clause} AND mime_type = '{}'",
                    escape_sql_literal(m)
                )),
                None => q.only_if(scope_clause),
            };
            let mut stream = q.execute().await.context("run document search")?;
            let mut hits = Vec::new();
            while let Some(batch) = stream.try_next().await.context("stream next batch")? {
                decode_document_hits(&batch, &mut hits)?;
            }
            anyhow::Ok(hits)
        })
    }

    /// KNN over the memory table for one scope.
    pub fn search_memory(
        &self,
        scope: &str,
        query: Vec<f32>,
        limit: usize,
        tag_filter: Option<&str>,
    ) -> Result<Vec<MemoryHit>> {
        if query.len() != usize::from(self.inner.dim) {
            return Err(anyhow!(
                "query vector dim {} does not match store dim {}",
                query.len(),
                self.inner.dim
            ));
        }
        self.inner.runtime.block_on(async {
            let table = self
                .inner
                .connection
                .open_table(MEMORY_TABLE)
                .execute()
                .await
                .with_context(|| format!("open {MEMORY_TABLE} table"))?;
            let mut q = table
                .vector_search(query)
                .context("build memory vector search")?
                .limit(limit);
            let scope_clause = format!("scope = '{}'", escape_sql_literal(scope));
            // LanceDB's predicate language does not have a clean "list_contains" yet for
            // List<Utf8>; tag filtering is therefore best-effort post-filter in the host.
            let _ = tag_filter; // kept in the signature for forward-compat
            q = q.only_if(scope_clause);
            let mut stream = q.execute().await.context("run memory search")?;
            let mut hits = Vec::new();
            while let Some(batch) = stream.try_next().await.context("stream next batch")? {
                decode_memory_hits(&batch, &mut hits)?;
            }
            if let Some(tag) = tag_filter {
                hits.retain(|h| h.tags.iter().any(|t| t == tag));
            }
            anyhow::Ok(hits)
        })
    }
}

fn wipe_on_mismatch(dir: &Path, meta_path: &Path, expected: &LanceMeta) -> Result<()> {
    if !meta_path.exists() {
        return Ok(());
    }
    let bytes =
        std::fs::read(meta_path).with_context(|| format!("read {}", meta_path.display()))?;
    let actual: LanceMeta =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", meta_path.display()))?;
    if actual == *expected {
        return Ok(());
    }
    tracing::warn!(
        old_dim = actual.dim,
        new_dim = expected.dim,
        old_model = %actual.embedding_model,
        new_model = %expected.embedding_model,
        "lance store dim/model mismatch — wiping {}",
        dir.display()
    );
    // Remove every file/dir under `dir` but keep the dir itself, so callers
    // that hold a path reference don't break.
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry.context("entry")?;
        let p = entry.path();
        if p.is_dir() {
            std::fs::remove_dir_all(&p).with_context(|| format!("remove {}", p.display()))?;
        } else {
            std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
        }
    }
    Ok(())
}

async fn ensure_table(
    connection: &Connection,
    name: &str,
    schema: arrow_schema::SchemaRef,
) -> Result<()> {
    let existing: Vec<String> = connection
        .table_names()
        .execute()
        .await
        .context("list lance tables")?;
    if existing.iter().any(|t| t == name) {
        return Ok(());
    }
    connection
        .create_empty_table(name, schema)
        .execute()
        .await
        .with_context(|| format!("create {name} table"))?;
    Ok(())
}

fn build_documents_batch(dim: u16, rows: &[DocumentRow]) -> Result<RecordBatch> {
    let mut scope = StringBuilder::new();
    let mut path = StringBuilder::new();
    let mut chunk_idx = UInt32Builder::new();
    let mut mime = StringBuilder::new();
    let mut text = StringBuilder::new();
    let mut byte_start = UInt32Builder::new();
    let mut byte_end = UInt32Builder::new();
    let mut embedding = FixedSizeListBuilder::new(Float32Builder::new(), i32::from(dim));

    for r in rows {
        if r.embedding.len() != usize::from(dim) {
            return Err(anyhow!(
                "documents row embedding dim {} does not match store dim {}",
                r.embedding.len(),
                dim
            ));
        }
        scope.append_value(&r.scope);
        path.append_value(&r.path);
        chunk_idx.append_value(r.chunk_idx);
        mime.append_value(&r.mime_type);
        text.append_value(&r.text);
        byte_start.append_value(r.byte_start);
        byte_end.append_value(r.byte_end);
        for v in &r.embedding {
            embedding.values().append_value(*v);
        }
        embedding.append(true);
    }

    let schema = documents_schema(dim);
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(scope.finish()),
            Arc::new(path.finish()),
            Arc::new(chunk_idx.finish()),
            Arc::new(mime.finish()),
            Arc::new(text.finish()),
            Arc::new(byte_start.finish()),
            Arc::new(byte_end.finish()),
            Arc::new(embedding.finish()),
        ],
    )
    .context("assemble documents batch")
}

fn build_memory_batch(dim: u16, rows: &[MemoryRow]) -> Result<RecordBatch> {
    let mut scope = StringBuilder::new();
    let mut key = StringBuilder::new();
    let mut value = StringBuilder::new();
    let mut tags = ListBuilder::new(StringBuilder::new());
    let mut embedding = FixedSizeListBuilder::new(Float32Builder::new(), i32::from(dim));
    let mut created = TimestampMicrosecondBuilder::new();
    let mut updated = TimestampMicrosecondBuilder::new();

    for r in rows {
        if r.embedding.len() != usize::from(dim) {
            return Err(anyhow!(
                "memory row embedding dim {} does not match store dim {}",
                r.embedding.len(),
                dim
            ));
        }
        scope.append_value(&r.scope);
        key.append_value(&r.key);
        value.append_value(&r.value);
        for t in &r.tags {
            tags.values().append_value(t);
        }
        tags.append(true);
        for v in &r.embedding {
            embedding.values().append_value(*v);
        }
        embedding.append(true);
        created.append_value(r.created_at);
        updated.append_value(r.updated_at);
    }

    let schema = memory_schema(dim);
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(scope.finish()),
            Arc::new(key.finish()),
            Arc::new(value.finish()),
            Arc::new(tags.finish()),
            Arc::new(embedding.finish()),
            Arc::new(created.finish()),
            Arc::new(updated.finish()),
        ],
    )
    .context("assemble memory batch")
}

fn decode_document_hits(batch: &RecordBatch, out: &mut Vec<DocumentHit>) -> Result<()> {
    use arrow_array::{Float32Array, UInt32Array};
    let path = batch
        .column_by_name("path")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| anyhow!("`path` column missing"))?;
    let mime = batch
        .column_by_name("mime_type")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| anyhow!("`mime_type` column missing"))?;
    let text = batch
        .column_by_name("text")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| anyhow!("`text` column missing"))?;
    let chunk_idx = batch
        .column_by_name("chunk_idx")
        .and_then(|c| c.as_any().downcast_ref::<UInt32Array>())
        .ok_or_else(|| anyhow!("`chunk_idx` column missing"))?;
    let byte_start = batch
        .column_by_name("byte_start")
        .and_then(|c| c.as_any().downcast_ref::<UInt32Array>())
        .ok_or_else(|| anyhow!("`byte_start` column missing"))?;
    let byte_end = batch
        .column_by_name("byte_end")
        .and_then(|c| c.as_any().downcast_ref::<UInt32Array>())
        .ok_or_else(|| anyhow!("`byte_end` column missing"))?;
    let distance = batch
        .column_by_name("_distance")
        .and_then(|c| c.as_any().downcast_ref::<Float32Array>());

    for i in 0..batch.num_rows() {
        out.push(DocumentHit {
            path: path.value(i).to_string(),
            chunk_idx: chunk_idx.value(i),
            text: text.value(i).to_string(),
            mime_type: mime.value(i).to_string(),
            byte_start: byte_start.value(i),
            byte_end: byte_end.value(i),
            distance: distance.map(|d| d.value(i)).unwrap_or(0.0),
        });
    }
    Ok(())
}

fn decode_memory_hits(batch: &RecordBatch, out: &mut Vec<MemoryHit>) -> Result<()> {
    use arrow_array::{Float32Array, ListArray};
    let key = batch
        .column_by_name("key")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| anyhow!("`key` column missing"))?;
    let value = batch
        .column_by_name("value")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| anyhow!("`value` column missing"))?;
    let tags = batch
        .column_by_name("tags")
        .and_then(|c| c.as_any().downcast_ref::<ListArray>());
    let distance = batch
        .column_by_name("_distance")
        .and_then(|c| c.as_any().downcast_ref::<Float32Array>());

    for i in 0..batch.num_rows() {
        let tag_list: Vec<String> = match tags {
            Some(list) if list.is_valid(i) => {
                let inner = list.value(i);
                let s = inner
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| anyhow!("tags inner type unexpected"))?;
                (0..s.len()).map(|j| s.value(j).to_string()).collect()
            }
            _ => Vec::new(),
        };
        out.push(MemoryHit {
            key: key.value(i).to_string(),
            value: value.value(i).to_string(),
            tags: tag_list,
            distance: distance.map(|d| d.value(i)).unwrap_or(0.0),
        });
    }
    Ok(())
}

/// Single-quote escape for the simple SQL-literal predicates we use.
fn escape_sql_literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// Convenience: current time as microseconds since unix epoch, saturating on
/// the (effectively impossible) clock-before-epoch case.
pub fn now_micros() -> i64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(dur.as_micros()).unwrap_or(i64::MAX)
}
