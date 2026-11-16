//! Arm B: LanceDB with scalar and full-text indexes on — BTree on `doc_key`, Bitmap on `mime_group`
//! and `lang`, LabelList on `entity_cats`, FTS on `text`. ADR-0029 rule 6 keeps the vector lane exact
//! and builds a vector index only where exact search misses its budget, so the IVF_HNSW_SQ index is
//! opt-in (`--ann`); with it, pre-filtered row counts at or below the threshold are still exact.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use lancedb::arrow::arrow_array::RecordBatch;
use lancedb::index::Index;
use lancedb::index::scalar::{FtsIndexBuilder, FullTextSearchQuery};
use lancedb::index::vector::IvfHnswSqIndexBuilder;
use lancedb::query::{QueryBase, Select};
use lancedb::table::{OptimizeAction, OptimizeOptions};

use crate::backend::{Backend, Filter, Hit, VectorMode};
use crate::lance_flat::{LanceTable, apply_filter};

/// Query-time knobs. `None` leaves Lance's default in force.
#[derive(Debug, Clone, Copy)]
pub struct AnnParams {
    /// Pre-filtered row counts at or below this are searched exactly (`bypass_vector_index`).
    pub exact_max_rows: usize,
    pub nprobes: Option<usize>,
    pub refine_factor: Option<u32>,
    pub ef: Option<usize>,
    /// IVF partitions at index build; `None` lets Lance choose.
    pub partitions: Option<u32>,
    /// Build the IVF_HNSW_SQ index in `build_indexes`.
    pub build_ann: bool,
}

pub struct LanceIndexed {
    store: LanceTable,
    params: AnnParams,
    has_ann: bool,
    /// Seconds spent per index in the last `build_indexes`.
    pub index_seconds: Vec<(&'static str, f64)>,
}

impl LanceIndexed {
    pub fn open(dir: &Path, dim: usize, current_thread: bool, params: AnnParams) -> Result<Self> {
        let store = LanceTable::open(dir, dim, current_thread)?;
        let has_ann = store
            .rt
            .block_on(store.table.list_indices())
            .context("list indices")?
            .iter()
            .any(|index| index.columns.iter().any(|column| column == "embedding"));
        Ok(Self {
            store,
            params,
            has_ann,
            index_seconds: Vec::new(),
        })
    }

    fn create(&mut self, label: &'static str, column: &str, index: Index) -> Result<()> {
        let started = Instant::now();
        self.store
            .rt
            .block_on(self.store.table.create_index(&[column], index).execute())
            .with_context(|| format!("create {label} index on {column}"))?;
        self.index_seconds.push((label, started.elapsed().as_secs_f64()));
        Ok(())
    }

    /// `(indexed, unindexed)` rows per index, to show what an append left outside each one.
    pub fn index_coverage(&self) -> Result<Vec<(String, usize, usize)>> {
        self.store.rt.block_on(async {
            let mut out = Vec::new();
            for index in self.store.table.list_indices().await? {
                if let Some(stats) = self.store.table.index_stats(&index.name).await? {
                    out.push((index.name, stats.num_indexed_rows, stats.num_unindexed_rows));
                }
            }
            Ok(out)
        })
    }
}

impl Backend for LanceIndexed {
    fn build(&mut self, rows: &mut dyn Iterator<Item = Result<RecordBatch>>) -> Result<()> {
        self.store.add(rows)
    }

    fn build_indexes(&mut self) -> Result<()> {
        self.index_seconds.clear();
        self.create("btree_doc_key", "doc_key", Index::BTree(Default::default()))?;
        self.create("bitmap_mime_group", "mime_group", Index::Bitmap(Default::default()))?;
        self.create("bitmap_lang", "lang", Index::Bitmap(Default::default()))?;
        self.create(
            "labellist_entity_cats",
            "entity_cats",
            Index::LabelList(Default::default()),
        )?;
        self.create("fts_text", "text", Index::FTS(FtsIndexBuilder::default()))?;
        if self.params.build_ann {
            let mut ann = IvfHnswSqIndexBuilder::default();
            if let Some(partitions) = self.params.partitions {
                ann = ann.num_partitions(partitions);
            }
            self.create("ivf_hnsw_sq_embedding", "embedding", Index::IvfHnswSq(ann))?;
            self.has_ann = true;
        }
        Ok(())
    }

    fn vector(&self, query: &[f32], filter: &Filter, k: usize) -> Result<(Vec<Hit>, VectorMode)> {
        let mut q = self.store.vector_query(query, filter, k)?;
        // Without a vector index every search is exact and the count is not needed.
        let over_threshold = self.has_ann
            && self
                .store
                .rt
                .block_on(self.store.table.count_rows(filter.sql()))
                .context("count pre-filtered rows")?
                > self.params.exact_max_rows;
        let mode = if over_threshold {
            if let Some(nprobes) = self.params.nprobes {
                q = q.nprobes(nprobes)?;
            }
            if let Some(refine_factor) = self.params.refine_factor {
                q = q.refine_factor(refine_factor);
            }
            if let Some(ef) = self.params.ef {
                q = q.ef(ef);
            }
            VectorMode::Ann
        } else {
            q = q.bypass_vector_index();
            VectorMode::Exact
        };
        Ok((self.store.collect(&q)?, mode))
    }

    fn lexical(&self, text: &str, filter: &Filter, k: usize) -> Result<Option<Vec<Hit>>> {
        let q = self
            .store
            .table
            .query()
            .full_text_search(FullTextSearchQuery::new(text.to_string()))
            .limit(k)
            .select(Select::columns(&["doc_key", "chunk_ord"]));
        Ok(Some(self.store.collect(&apply_filter(q, filter))?))
    }

    fn append(&mut self, rows: &mut dyn Iterator<Item = Result<RecordBatch>>) -> Result<()> {
        self.store.add(rows)
    }

    fn optimize(&mut self) -> Result<()> {
        self.store
            .rt
            .block_on(
                self.store
                    .table
                    .optimize(OptimizeAction::Index(OptimizeOptions::default())),
            )
            .context("optimize indexes")?;
        Ok(())
    }
}
