//! The `memory` table: one vector row per saved note.
//!
//! The Fjall `memory_by_key` record is the truth for a note; a row here only makes it findable by
//! meaning. A caller must join every hit against that record (see `crate::mcp::memory_vectors`).

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use futures::TryStreamExt;
use lancedb::arrow::arrow_array::builder::{
    FixedSizeListBuilder, Float32Builder, ListBuilder, StringBuilder, TimestampMicrosecondBuilder,
};
use lancedb::arrow::arrow_array::{Array, RecordBatch, StringArray};
use lancedb::query::{ExecutableQuery, QueryBase};

use super::schema::{MEMORY_TABLE, memory_schema};
use super::{LanceStore, MemoryHit, MemoryRow, memory_namespace_predicate, memory_row_predicate};

impl LanceStore {
    /// Insert / upsert one memory row keyed by `(scope, key)`. Existing rows
    /// with the same key are removed first.
    pub fn upsert_memory(&self, row: MemoryRow) -> Result<()> {
        self.inner.rt().block_on(async {
            let table = self
                .inner
                .connection
                .open_table(MEMORY_TABLE)
                .execute()
                .await
                .with_context(|| format!("open {MEMORY_TABLE} table"))?;
            let predicate = memory_row_predicate(&row.scope, &row.visibility, &row.agent_id, &row.key);
            table.delete(&predicate).await.context("delete previous memory entry")?;
            let batch = build_memory_batch(self.inner.dim, std::slice::from_ref(&row))?;
            table.add(batch).execute().await.context("insert memory row")?;
            self.after_commit(&table, MEMORY_TABLE).await;
            anyhow::Ok(())
        })
    }

    /// Delete one memory entry by `(scope, visibility, agent_id, key)`. Returns the
    /// number of rows LanceDB actually deleted (`0` when no row matched the predicate).
    pub fn delete_memory(&self, scope: &str, visibility: &str, agent_id: &str, key: &str) -> Result<u64> {
        self.inner.rt().block_on(async {
            let table = self
                .inner
                .connection
                .open_table(MEMORY_TABLE)
                .execute()
                .await
                .with_context(|| format!("open {MEMORY_TABLE} table"))?;
            let predicate = memory_row_predicate(scope, visibility, agent_id, key);
            let result = table.delete(&predicate).await.context("delete memory entry")?;
            anyhow::Ok(result.num_deleted_rows)
        })
    }

    /// `key -> updated_at` of every row in one `(scope, visibility, agent_id)` namespace: which
    /// write of each note the table holds a vector for.
    pub fn memory_stamps(&self, scope: &str, visibility: &str, agent_id: &str) -> Result<ahash::AHashMap<String, i64>> {
        use lancedb::arrow::arrow_array::TimestampMicrosecondArray;
        use lancedb::query::Select;

        self.inner.rt().block_on(async {
            let table = self
                .inner
                .connection
                .open_table(MEMORY_TABLE)
                .execute()
                .await
                .with_context(|| format!("open {MEMORY_TABLE} table"))?;
            let mut stream = table
                .query()
                .only_if(memory_namespace_predicate(scope, visibility, agent_id))
                .select(Select::columns(&["key", "updated_at"]))
                .execute()
                .await
                .context("list memory rows")?;
            let mut stamps = ahash::AHashMap::new();
            while let Some(batch) = stream.try_next().await.context("stream next batch")? {
                let key = batch
                    .column_by_name("key")
                    .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                    .ok_or_else(|| anyhow!("`key` column missing"))?;
                let updated = batch
                    .column_by_name("updated_at")
                    .and_then(|c| c.as_any().downcast_ref::<TimestampMicrosecondArray>())
                    .ok_or_else(|| anyhow!("`updated_at` column missing"))?;
                for i in 0..batch.num_rows() {
                    stamps.insert(key.value(i).to_string(), updated.value(i));
                }
            }
            anyhow::Ok(stamps)
        })
    }

    /// KNN over the memory table for one `(scope, visibility, agent_id)` namespace.
    ///
    /// The `visibility` + `agent_id` predicate is mandatory so an individual search never
    /// returns another agent's rows and a group search only sees group rows.
    pub fn search_memory(
        &self,
        scope: &str,
        visibility: &str,
        agent_id: &str,
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
        self.inner.rt().block_on(async {
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
            let namespace_clause = memory_namespace_predicate(scope, visibility, agent_id);
            let _ = tag_filter;
            q = q.only_if(namespace_clause);
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

fn build_memory_batch(dim: u16, rows: &[MemoryRow]) -> Result<RecordBatch> {
    let mut scope = StringBuilder::new();
    let mut key = StringBuilder::new();
    let mut value = StringBuilder::new();
    let mut tags = ListBuilder::new(StringBuilder::new());
    let mut visibility = StringBuilder::new();
    let mut agent_id = StringBuilder::new();
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
        visibility.append_value(&r.visibility);
        agent_id.append_value(&r.agent_id);
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
            Arc::new(visibility.finish()),
            Arc::new(agent_id.finish()),
            Arc::new(embedding.finish()),
            Arc::new(created.finish()),
            Arc::new(updated.finish()),
        ],
    )
    .context("assemble memory batch")
}

fn decode_memory_hits(batch: &RecordBatch, out: &mut Vec<MemoryHit>) -> Result<()> {
    use lancedb::arrow::arrow_array::{Float32Array, ListArray, TimestampMicrosecondArray};
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
    let updated = batch
        .column_by_name("updated_at")
        .and_then(|c| c.as_any().downcast_ref::<TimestampMicrosecondArray>())
        .ok_or_else(|| anyhow!("`updated_at` column missing"))?;
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
            updated_at: updated.value(i),
            distance: distance.map(|d| d.value(i)).unwrap_or(0.0),
        });
    }
    Ok(())
}
