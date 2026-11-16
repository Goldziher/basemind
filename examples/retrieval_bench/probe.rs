//! Experiments that settle what the design could not determine by reading Lance 14.0.2. Each prints
//! one JSON object; `benchmarks/retrieval/PROBES.md` records the answers.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use ahash::AHashMap;
use anyhow::{Context, Result};
use lancedb::arrow::arrow_array::builder::{FixedSizeListBuilder, Float32Builder};
use lancedb::arrow::arrow_array::{ArrayRef, RecordBatch};
use lancedb::expr::{col, is_in, lit};
use lancedb::index::Index;
use lancedb::index::scalar::FullTextSearchQuery;
use lancedb::index::vector::IvfHnswSqIndexBuilder;
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use serde_json::{Value, json};

use crate::backend::{Filter, Hit};
use crate::corpus::{CHUNKS_PER_DOC, Columns, Corpus, Params, doc_key};
use crate::lance_flat::LanceTable;
use crate::truth::{load_queries, percentile};

const PROBE_DIM: usize = 16;
const PROBE_SEED: u64 = 7;
const REPEATS: usize = 15;

fn outcome(result: Result<Vec<Hit>>, null_rows: &AHashMap<String, Vec<u32>>) -> Value {
    match result {
        Ok(hits) => {
            let from_null = hits
                .iter()
                .filter(|hit| null_rows.get(&hit.doc_key).is_some_and(|ords| ords.contains(&hit.ord)))
                .count();
            json!({"ok": true, "hits": hits.len(), "hits_with_null_embedding": from_null})
        }
        Err(error) => json!({"ok": false, "error": format!("{error:#}")}),
    }
}

/// A corpus batch whose odd rows carry a null embedding.
fn half_null(batch: &RecordBatch, dim: usize) -> Result<RecordBatch> {
    let columns = Columns::of(batch)?;
    let mut embedding = FixedSizeListBuilder::new(Float32Builder::new(), dim as i32);
    for row in 0..batch.num_rows() {
        embedding
            .values()
            .append_slice(&columns.vectors[row * dim..(row + 1) * dim]);
        embedding.append(row % 2 == 0);
    }
    let mut arrays: Vec<ArrayRef> = batch.columns().to_vec();
    let last = arrays.len() - 1;
    arrays[last] = Arc::new(embedding.finish());
    Ok(RecordBatch::try_new(batch.schema(), arrays)?)
}

/// Does a vector search tolerate null embeddings, flat and through an ANN index?
pub fn nulls(dir: &Path) -> Result<Value> {
    let rows = 4000usize;
    let corpus = Corpus::new(Params {
        n: rows as u64,
        dim: PROBE_DIM,
        seed: PROBE_SEED,
    });
    let store = LanceTable::open(dir, PROBE_DIM, false)?;
    let batch = half_null(&corpus.batch(0, rows)?, PROBE_DIM)?;
    store.add(&mut std::iter::once(Ok(batch)))?;
    let mut null_rows: AHashMap<String, Vec<u32>> = AHashMap::new();
    for row in (1..rows as u64).step_by(2) {
        null_rows
            .entry(doc_key(PROBE_SEED, row / CHUNKS_PER_DOC))
            .or_default()
            .push((row % CHUNKS_PER_DOC) as u32);
    }
    let query = corpus.row_vector(0);
    let search = |k: usize, sql: Option<&str>, bypass: bool| -> Value {
        let run = || -> Result<Vec<Hit>> {
            let mut q = store.vector_query(&query, &Filter::default(), k)?;
            if let Some(sql) = sql {
                q = q.only_if(sql);
            }
            if bypass {
                q = q.bypass_vector_index();
            }
            store.collect(&q)
        };
        outcome(run(), &null_rows)
    };
    let flat = json!({
        "k10": search(10, None, true),
        "k_all": search(rows, None, true),
        "k_all_is_not_null": search(rows, Some("embedding IS NOT NULL"), true),
    });
    let build = store.rt.block_on(
        store
            .table
            .create_index(
                &["embedding"],
                Index::IvfHnswSq(IvfHnswSqIndexBuilder::default().num_partitions(4)),
            )
            .execute(),
    );
    let ann = match build {
        Ok(()) => json!({
            "index_build": "ok",
            "k10": search(10, None, false),
            "k_all": search(rows, None, false),
        }),
        Err(error) => json!({"index_build": format!("{error:#}")}),
    };
    let null_count = store
        .rt
        .block_on(store.table.count_rows(Some("embedding IS NULL".to_string())))?;
    Ok(json!({"probe": "nulls", "rows": rows, "null_embeddings": null_count, "flat": flat, "ann": ann}))
}

/// What does `full_text_search` do on a table with no FTS index?
pub fn fts_no_index(dir: &Path) -> Result<Value> {
    let rows = 20_000usize;
    let corpus = Corpus::new(Params {
        n: rows as u64,
        dim: PROBE_DIM,
        seed: PROBE_SEED,
    });
    let store = LanceTable::open(dir, PROBE_DIM, false)?;
    store.add(&mut (0..rows).step_by(4000).map(|start| corpus.batch(start as u64, 4000)))?;
    let text = corpus.queries()[0].text.clone();
    let run = |store: &LanceTable| -> Value {
        let started = Instant::now();
        let q = store
            .table
            .query()
            .full_text_search(FullTextSearchQuery::new(text.clone()))
            .limit(10)
            .select(Select::columns(&["doc_key", "chunk_ord"]));
        match store.collect(&q) {
            Ok(hits) => json!({"ok": true, "hits": hits.len(), "ms": started.elapsed().as_secs_f64() * 1e3}),
            Err(error) => json!({"ok": false, "error": format!("{error:#}")}),
        }
    };
    let without = run(&store);
    store
        .rt
        .block_on(
            store
                .table
                .create_index(&["text"], Index::FTS(Default::default()))
                .execute(),
        )
        .context("create FTS index")?;
    Ok(
        json!({"probe": "fts_no_index", "rows": rows, "query": text, "without_index": without, "with_index": run(&store)}),
    )
}

fn median_ms(mut run: impl FnMut() -> Result<()>) -> Result<f64> {
    let mut samples = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let started = Instant::now();
        run()?;
        samples.push(started.elapsed().as_secs_f64() * 1e3);
    }
    samples.sort_by(f64::total_cmp);
    Ok(percentile(&samples, 50.0))
}

/// Cost of a `doc_key IN (…)` pre-filter of `size` keys on an existing Lance store.
pub fn inlist(corpus_dir: &Path, store_dir: &Path, size: usize) -> Result<Value> {
    let params = Params::load(corpus_dir)?;
    let query = load_queries(corpus_dir)?.swap_remove(0).vector;
    let store = LanceTable::open(store_dir, params.dim, false)?;
    let docs = params.n / CHUNKS_PER_DOC;
    let keys: Vec<String> = (0..size as u64)
        .map(|i| doc_key(params.seed, i * docs / size as u64))
        .collect();
    let sql = format!(
        "doc_key IN ({})",
        keys.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(", ")
    );
    let matched = store.rt.block_on(store.table.count_rows(Some(sql.clone())))?;
    let count_ms = median_ms(|| {
        store.rt.block_on(store.table.count_rows(Some(sql.clone())))?;
        Ok(())
    })?;
    let sql_ms = median_ms(|| {
        let q = store
            .vector_query(&query, &Filter::default(), 200)?
            .only_if(&sql)
            .bypass_vector_index();
        store.collect(&q).map(drop)
    })?;
    let expr_ms = median_ms(|| {
        let list = keys.iter().map(|k| lit(k.as_str())).collect();
        let q = store
            .vector_query(&query, &Filter::default(), 200)?
            .only_if_expr(is_in(col("doc_key"), list))
            .bypass_vector_index();
        store.collect(&q).map(drop)
    })?;
    let indexed = store
        .rt
        .block_on(store.table.list_indices())?
        .iter()
        .any(|index| index.columns.iter().any(|c| c == "doc_key"));
    let plan = store.rt.block_on(
        store
            .vector_query(&query, &Filter::default(), 200)?
            .only_if(&sql)
            .bypass_vector_index()
            .explain_plan(false),
    )?;
    let plan_head: String = plan
        .lines()
        .map(|line| line.chars().take(160).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(json!({
        "probe": "inlist", "n": params.n, "keys": size, "matched_rows": matched,
        "doc_key_indexed": indexed, "sql_bytes": sql.len(), "count_rows_p50_ms": count_ms,
        "exact_search_sql_p50_ms": sql_ms, "exact_search_expr_p50_ms": expr_ms, "plan": plan_head,
    }))
}
