//! Persistence of the document→code links (ADR-0008) in the LanceDB document store.
//!
//! Split out of `lance/mod.rs` to keep that module under the 1000-line cap. A link is a raw mention
//! (`name` or `path`) carried by one chunk of a document; resolution to a graph node is deferred to
//! the codegraph `documents` build lane. The table is vector-free and dim-independent — see
//! [`super::schema::doc_links_schema`].

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use arrow_array::builder::{StringBuilder, UInt32Builder};
use arrow_array::{RecordBatch, StringArray};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use super::{LanceStore, escape_sql_literal, schema};

/// One row in the `doc_links` table — a persisted document→code link (ADR-0008).
#[derive(Debug, Clone)]
pub struct DocLinkRow {
    pub scope: String,
    pub doc_path: String,
    pub chunk_idx: u32,
    /// `"name"` (identifier / keyword / entity) or `"path"` (repo-relative path citation).
    pub mention_kind: String,
    pub mention_value: String,
}

impl LanceStore {
    /// Replace all `doc_links` rows for a `(scope, doc_path)` pair and insert the supplied rows
    /// (ADR-0008). Delete-then-insert mirrors [`Self::replace_document`] so a re-scan never leaves a
    /// stale link behind. An empty `rows` deletes the document's links without re-inserting.
    pub fn replace_doc_links(&self, scope: &str, doc_path: &str, rows: Vec<DocLinkRow>) -> Result<()> {
        self.inner.rt().block_on(async {
            let table = self
                .inner
                .connection
                .open_table(schema::DOC_LINKS_TABLE)
                .execute()
                .await
                .with_context(|| format!("open {} table", schema::DOC_LINKS_TABLE))?;
            let predicate = format!(
                "scope = '{}' AND doc_path = '{}'",
                escape_sql_literal(scope),
                escape_sql_literal(doc_path)
            );
            table
                .delete(&predicate)
                .await
                .with_context(|| format!("delete existing doc_links for {scope}/{doc_path}"))?;
            if rows.is_empty() {
                return Ok(());
            }
            let batch = build_doc_links_batch(&rows)?;
            table
                .add(batch)
                .execute()
                .await
                .with_context(|| format!("insert {} doc_links rows", rows.len()))?;
            anyhow::Ok(())
        })
    }

    /// Read every `doc_links` row for one ingestion `scope` (ADR-0008). Used by the serve cache-warm
    /// path to reload persisted document→code links into the in-RAM map.
    pub fn all_doc_links(&self, scope: &str) -> Result<Vec<DocLinkRow>> {
        self.inner.rt().block_on(async {
            let table = self
                .inner
                .connection
                .open_table(schema::DOC_LINKS_TABLE)
                .execute()
                .await
                .with_context(|| format!("open {} table", schema::DOC_LINKS_TABLE))?;
            let mut stream = table
                .query()
                .only_if(format!("scope = '{}'", escape_sql_literal(scope)))
                .execute()
                .await
                .context("run doc_links query")?;
            let mut out = Vec::new();
            while let Some(batch) = stream.try_next().await.context("stream next batch")? {
                decode_doc_link_rows(scope, &batch, &mut out)?;
            }
            anyhow::Ok(out)
        })
    }
}

fn build_doc_links_batch(rows: &[DocLinkRow]) -> Result<RecordBatch> {
    let mut scope = StringBuilder::new();
    let mut doc_path = StringBuilder::new();
    let mut chunk_idx = UInt32Builder::new();
    let mut mention_kind = StringBuilder::new();
    let mut mention_value = StringBuilder::new();

    for r in rows {
        scope.append_value(&r.scope);
        doc_path.append_value(&r.doc_path);
        chunk_idx.append_value(r.chunk_idx);
        mention_kind.append_value(&r.mention_kind);
        mention_value.append_value(&r.mention_value);
    }

    RecordBatch::try_new(
        schema::doc_links_schema(),
        vec![
            Arc::new(scope.finish()),
            Arc::new(doc_path.finish()),
            Arc::new(chunk_idx.finish()),
            Arc::new(mention_kind.finish()),
            Arc::new(mention_value.finish()),
        ],
    )
    .context("assemble doc_links batch")
}

fn decode_doc_link_rows(scope: &str, batch: &RecordBatch, out: &mut Vec<DocLinkRow>) -> Result<()> {
    use arrow_array::UInt32Array;
    let str_col = |name: &str| -> Result<&StringArray> {
        batch
            .column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .ok_or_else(|| anyhow!("`{name}` column missing"))
    };
    let doc_path = str_col("doc_path")?;
    let mention_kind = str_col("mention_kind")?;
    let mention_value = str_col("mention_value")?;
    let chunk_idx = batch
        .column_by_name("chunk_idx")
        .and_then(|c| c.as_any().downcast_ref::<UInt32Array>())
        .ok_or_else(|| anyhow!("`chunk_idx` column missing"))?;

    for i in 0..batch.num_rows() {
        out.push(DocLinkRow {
            scope: scope.to_string(),
            doc_path: doc_path.value(i).to_string(),
            chunk_idx: chunk_idx.value(i),
            mention_kind: mention_kind.value(i).to_string(),
            mention_value: mention_value.value(i).to_string(),
        });
    }
    Ok(())
}
