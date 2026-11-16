//! Arm A: LanceDB as basemind uses it today — the target schema, no index of any kind, every vector
//! query a filtered flat scan. Also the Lance plumbing arm B shares.

use std::path::Path;

use anyhow::{Context, Result};
use futures::TryStreamExt;
use lancedb::arrow::arrow_array::{RecordBatch, StringArray, UInt32Array};
use lancedb::expr::{col, is_in, lit};
use lancedb::query::{ExecutableQuery, QueryBase, Select, VectorQuery};
use lancedb::table::{CompactionOptions, OptimizeAction};
use lancedb::{DistanceType, Table};
use tokio::runtime::Runtime;

use crate::backend::{Backend, Filter, Hit, VectorMode};
use crate::corpus::chunk_schema;

pub const TABLE: &str = "doc_chunks";
/// Batches handed to one `Table::add`: about 262k rows, so a commit holds well under 1 GB of vectors.
const BATCHES_PER_COMMIT: usize = 64;

/// A Lance table plus the runtime its async client is driven on.
pub struct LanceTable {
    pub rt: Runtime,
    pub table: Table,
}

impl LanceTable {
    /// `current_thread` reproduces `src/lance/mod.rs` (`LanceStore::open` builds a current-thread
    /// runtime); the default multi-thread runtime is the best case for Lance.
    pub fn open(dir: &Path, dim: usize, current_thread: bool) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let rt = if current_thread {
            tokio::runtime::Builder::new_current_thread().enable_all().build()?
        } else {
            tokio::runtime::Builder::new_multi_thread().enable_all().build()?
        };
        let uri = dir.to_string_lossy().into_owned();
        let table = rt.block_on(async {
            let connection = lancedb::connect(&uri).execute().await.context("connect")?;
            let names = connection.table_names().execute().await.context("list tables")?;
            if names.iter().any(|name| name == TABLE) {
                connection.open_table(TABLE).execute().await.context("open table")
            } else {
                connection
                    .create_empty_table(TABLE, chunk_schema(dim))
                    .execute()
                    .await
                    .context("create table")
            }
        })?;
        Ok(Self { rt, table })
    }

    pub fn add(&self, rows: &mut dyn Iterator<Item = Result<RecordBatch>>) -> Result<()> {
        let mut pending = Vec::with_capacity(BATCHES_PER_COMMIT);
        loop {
            let next = rows.next().transpose()?;
            let done = next.is_none();
            pending.extend(next);
            if pending.len() == BATCHES_PER_COMMIT || (done && !pending.is_empty()) {
                let commit = std::mem::take(&mut pending);
                self.rt.block_on(self.table.add(commit).execute()).context("add rows")?;
            }
            if done {
                return Ok(());
            }
        }
    }

    pub fn vector_query(&self, query: &[f32], filter: &Filter, k: usize) -> Result<VectorQuery> {
        let q = self
            .table
            .vector_search(query)
            .context("build vector search")?
            .column("embedding")
            .distance_type(DistanceType::L2)
            .limit(k)
            .select(Select::columns(&["doc_key", "chunk_ord"]));
        Ok(apply_filter(q, filter))
    }

    pub fn collect(&self, query: &impl ExecutableQuery) -> Result<Vec<Hit>> {
        self.rt.block_on(async {
            let mut stream = query.execute().await.context("execute query")?;
            let mut hits = Vec::new();
            while let Some(batch) = stream.try_next().await.context("next batch")? {
                decode_hits(&batch, &mut hits)?;
            }
            Ok(hits)
        })
    }
}

/// The metadata filter as SQL and the live-key list as a DataFusion `InList` expression
/// (`lancedb-0.41.0/src/query.rs`, `only_if_expr`), so a list of thousands of keys is not re-parsed.
pub fn apply_filter<Q: QueryBase>(mut query: Q, filter: &Filter) -> Q {
    if let Some(sql) = filter.group_sql() {
        query = query.only_if(sql);
    }
    if let Some(keys) = &filter.keys {
        let list = keys.iter().map(|key| lit(key.as_str())).collect();
        query = query.only_if_expr(is_in(col("doc_key"), list));
    }
    query
}

pub fn decode_hits(batch: &RecordBatch, out: &mut Vec<Hit>) -> Result<()> {
    let key = batch
        .column_by_name("doc_key")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .context("`doc_key` column missing")?;
    let ord = batch
        .column_by_name("chunk_ord")
        .and_then(|c| c.as_any().downcast_ref::<UInt32Array>())
        .context("`chunk_ord` column missing")?;
    out.extend((0..batch.num_rows()).map(|i| Hit {
        doc_key: key.value(i).to_string(),
        ord: ord.value(i),
    }));
    Ok(())
}

pub struct LanceFlat {
    store: LanceTable,
}

impl LanceFlat {
    pub fn open(dir: &Path, dim: usize, current_thread: bool) -> Result<Self> {
        Ok(Self {
            store: LanceTable::open(dir, dim, current_thread)?,
        })
    }
}

impl Backend for LanceFlat {
    fn build(&mut self, rows: &mut dyn Iterator<Item = Result<RecordBatch>>) -> Result<()> {
        self.store.add(rows)
    }

    fn build_indexes(&mut self) -> Result<()> {
        Ok(())
    }

    fn vector(&self, query: &[f32], filter: &Filter, k: usize) -> Result<(Vec<Hit>, VectorMode)> {
        let q = self.store.vector_query(query, filter, k)?;
        Ok((self.store.collect(&q)?, VectorMode::Exact))
    }

    fn lexical(&self, _text: &str, _filter: &Filter, _k: usize) -> Result<Option<Vec<Hit>>> {
        Ok(None)
    }

    fn append(&mut self, rows: &mut dyn Iterator<Item = Result<RecordBatch>>) -> Result<()> {
        self.store.add(rows)
    }

    /// What `src/lance/batch.rs` (`optimize_table`) does after a scan: compact, no index upkeep.
    fn optimize(&mut self) -> Result<()> {
        self.store
            .rt
            .block_on(self.store.table.optimize(OptimizeAction::Compact {
                options: CompactionOptions::default(),
                remap_options: None,
            }))
            .context("compact")?;
        Ok(())
    }
}
