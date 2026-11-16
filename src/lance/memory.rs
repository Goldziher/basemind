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
use lancedb::arrow::arrow_array::{RecordBatch, StringArray};
use lancedb::query::{ExecutableQuery, QueryBase};

use super::schema::{MEMORY_TABLE, memory_schema};
use super::{LanceStore, MemoryHit, MemoryRow, escape_sql_literal, memory_namespace_predicate};

/// The rows of one memory namespace, by key: the `updated_at` each was embedded from, and the keys
/// that have more than one row (two processes upserting the same note at once).
#[derive(Debug, Default)]
pub struct MemoryStamps {
    pub stamps: ahash::AHashMap<String, i64>,
    pub duplicated: ahash::AHashSet<String>,
}

/// Predicate selecting the rows of `keys` in one `(scope, visibility, agent_id)` namespace.
pub(super) fn memory_keys_predicate(scope: &str, visibility: &str, agent_id: &str, keys: &[&str]) -> String {
    let list = keys
        .iter()
        .map(|key| format!("'{}'", escape_sql_literal(key)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{} AND key IN ({list})",
        memory_namespace_predicate(scope, visibility, agent_id)
    )
}

impl LanceStore {
    /// Insert / upsert one memory row keyed by `(scope, visibility, agent_id, key)`.
    pub fn upsert_memory(&self, row: MemoryRow) -> Result<()> {
        self.upsert_memory_rows(std::slice::from_ref(&row))
    }

    /// Upsert the rows of ONE `(scope, visibility, agent_id)` namespace: one delete, then one
    /// insert. For each key the delete takes the rows embedded from the same write or an earlier
    /// one, so it removes the duplicates two racing writers left, and run again with the same rows
    /// it leaves the same table. A row embedded from a LATER write stays: a caller that embedded
    /// from a listing may be behind a save made since, and must not remove that save's row.
    pub fn upsert_memory_rows(&self, rows: &[MemoryRow]) -> Result<()> {
        let Some(first) = rows.first() else {
            return Ok(());
        };
        if rows
            .iter()
            .any(|r| r.scope != first.scope || r.visibility != first.visibility || r.agent_id != first.agent_id)
        {
            return Err(anyhow!("memory rows of more than one namespace in one upsert"));
        }
        let superseded = rows
            .iter()
            .map(|row| {
                format!(
                    "(key = '{}' AND updated_at <= to_timestamp_micros({}))",
                    escape_sql_literal(&row.key),
                    row.updated_at
                )
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        let predicate = format!(
            "{} AND ({superseded})",
            memory_namespace_predicate(&first.scope, &first.visibility, &first.agent_id)
        );
        let batch = build_memory_batch(self.inner.dim, rows)?;
        self.inner.rt().block_on(async {
            let table = self.memory_table().await?;
            table.delete(&predicate).await.context("delete previous memory rows")?;
            table.add(batch).execute().await.context("insert memory rows")?;
            self.after_commit(&table, MEMORY_TABLE).await;
            anyhow::Ok(())
        })
    }

    /// Insert rows without removing the ones their keys already have: what a writer racing another
    /// leaves behind.
    #[cfg(test)]
    pub(crate) fn add_memory_rows_unchecked(&self, rows: &[MemoryRow]) -> Result<()> {
        let batch = build_memory_batch(self.inner.dim, rows)?;
        self.inner.rt().block_on(async {
            let table = self.memory_table().await?;
            table.add(batch).execute().await.context("insert memory rows")?;
            anyhow::Ok(())
        })
    }

    /// Delete one memory entry by `(scope, visibility, agent_id, key)`. Returns the
    /// number of rows LanceDB actually deleted (`0` when no row matched the predicate).
    pub fn delete_memory(&self, scope: &str, visibility: &str, agent_id: &str, key: &str) -> Result<u64> {
        self.delete_memory_keys(scope, visibility, agent_id, &[key], None)
    }

    /// Delete the rows of `keys` in one namespace with one predicate. With `written_by`, only rows
    /// whose `updated_at` is at or before that instant (microseconds) go: a caller that decided
    /// from an earlier listing must not remove a row written since.
    pub fn delete_memory_keys(
        &self,
        scope: &str,
        visibility: &str,
        agent_id: &str,
        keys: &[&str],
        written_by: Option<i64>,
    ) -> Result<u64> {
        if keys.is_empty() {
            return Ok(0);
        }
        let mut predicate = memory_keys_predicate(scope, visibility, agent_id, keys);
        if let Some(micros) = written_by {
            predicate.push_str(&format!(" AND updated_at <= to_timestamp_micros({micros})"));
        }
        self.inner.rt().block_on(async {
            let table = self.memory_table().await?;
            let result = table.delete(&predicate).await.context("delete memory rows")?;
            anyhow::Ok(result.num_deleted_rows)
        })
    }

    async fn memory_table(&self) -> Result<lancedb::Table> {
        self.inner
            .connection
            .open_table(MEMORY_TABLE)
            .execute()
            .await
            .with_context(|| format!("open {MEMORY_TABLE} table"))
    }

    /// Which write of each note one `(scope, visibility, agent_id)` namespace holds a vector for.
    pub fn memory_stamps(&self, scope: &str, visibility: &str, agent_id: &str) -> Result<MemoryStamps> {
        use lancedb::arrow::arrow_array::TimestampMicrosecondArray;
        use lancedb::query::Select;

        self.inner.rt().block_on(async {
            let table = self.memory_table().await?;
            let mut stream = table
                .query()
                .only_if(memory_namespace_predicate(scope, visibility, agent_id))
                .select(Select::columns(&["key", "updated_at"]))
                .execute()
                .await
                .context("list memory rows")?;
            let mut found = MemoryStamps::default();
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
                    if found
                        .stamps
                        .insert(key.value(i).to_string(), updated.value(i))
                        .is_some()
                    {
                        found.duplicated.insert(key.value(i).to_string());
                    }
                }
            }
            anyhow::Ok(found)
        })
    }

    /// KNN over the memory table for one `(scope, visibility, agent_id)` namespace, optionally
    /// only rows carrying `tag`.
    ///
    /// The `visibility` + `agent_id` predicate is mandatory so an individual search never
    /// returns another agent's rows and a group search only sees group rows. A row's tags are the
    /// note's as of the write the row was embedded from, so the `tag` filter is exact for every
    /// row the caller's join against the notes keeps.
    pub fn search_memory(
        &self,
        scope: &str,
        visibility: &str,
        agent_id: &str,
        query: Vec<f32>,
        limit: usize,
        tag: Option<&str>,
    ) -> Result<Vec<MemoryHit>> {
        use lancedb::query::Select;

        if query.len() != usize::from(self.inner.dim) {
            return Err(anyhow!(
                "query vector dim {} does not match store dim {}",
                query.len(),
                self.inner.dim
            ));
        }
        let mut predicate = memory_namespace_predicate(scope, visibility, agent_id);
        if let Some(tag) = tag {
            predicate.push_str(&format!(" AND array_has(tags, '{}')", escape_sql_literal(tag)));
        }
        self.inner.rt().block_on(async {
            let table = self.memory_table().await?;
            let mut stream = table
                .vector_search(query)
                .context("build memory vector search")?
                .limit(limit)
                .only_if(predicate)
                .select(Select::columns(&["key", "updated_at", "_distance"]))
                .execute()
                .await
                .context("run memory search")?;
            let mut hits = Vec::new();
            while let Some(batch) = stream.try_next().await.context("stream next batch")? {
                decode_memory_hits(&batch, &mut hits)?;
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
        // The column stays for stores created with it; the text is never written to it.
        value.append_value("");
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
    use lancedb::arrow::arrow_array::{Float32Array, TimestampMicrosecondArray};
    let key = batch
        .column_by_name("key")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| anyhow!("`key` column missing"))?;
    let updated = batch
        .column_by_name("updated_at")
        .and_then(|c| c.as_any().downcast_ref::<TimestampMicrosecondArray>())
        .ok_or_else(|| anyhow!("`updated_at` column missing"))?;
    let distance = batch
        .column_by_name("_distance")
        .and_then(|c| c.as_any().downcast_ref::<Float32Array>());

    for i in 0..batch.num_rows() {
        out.push(MemoryHit {
            key: key.value(i).to_string(),
            updated_at: updated.value(i),
            distance: distance.map(|d| d.value(i)).unwrap_or(0.0),
        });
    }
    Ok(())
}
