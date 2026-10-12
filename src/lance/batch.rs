//! Batched writes and version maintenance for the LanceDB store.
//!
//! Every LanceDB `delete` / `add` is one dataset commit, i.e. one new table version plus a manifest
//! write. The per-file `replace_*` writers pay two commits per document, so a 14 664-document scan
//! produced ~28k versions per table, 28 GB on disk and ~2 h of pure commit latency (issue #76). The
//! `replace_*_many` writers here fold N documents into ONE delete (`path IN (...)`) plus ONE append,
//! and [`LanceStore::maintain`] / the commit-counting auto-maintenance compact small fragments and
//! prune old versions so the version count stays bounded on the incremental / watch path too.

use std::time::Duration;

use anyhow::{Context, Result};
use lancedb::Table;
use lancedb::table::optimize::Duration as ChronoDuration;
use lancedb::table::{CompactionOptions, OptimizeAction};

use super::{DOCUMENTS_TABLE, DocumentRow, LanceStore, build_documents_batch, escape_sql_literal};

/// Documents folded into one delete+append commit pair.
pub const BATCH_MAX_DOCS: usize = 256;
/// Row cap per commit pair; bounds the Arrow batch (rows x dim x 4 B) held in RAM.
pub const BATCH_MAX_ROWS: usize = 4_096;

/// Auto-maintenance: prune a table's old versions every this many commits...
const AUTO_PRUNE_EVERY: u64 = 32;
/// ...and also compact its fragments every this many commits.
const AUTO_COMPACT_EVERY: u64 = 128;
/// Versions younger than this survive an auto prune (in-flight readers pin a version).
#[cfg(not(test))]
const AUTO_RETENTION: Duration = Duration::from_secs(300);
/// Tests prune everything not pinned, so the bound is observable without sleeping.
#[cfg(test)]
const AUTO_RETENTION: Duration = Duration::ZERO;
/// Versions younger than this survive the post-scan [`LanceStore::maintain`] pass.
pub const SCAN_RETENTION: Duration = Duration::from_secs(60);

/// Accumulator the scanner lanes use to turn a per-file stream into bounded batches.
pub struct PendingBatch<R> {
    items: Vec<(String, String, Vec<R>)>,
    rows: usize,
}

impl<R> Default for PendingBatch<R> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            rows: 0,
        }
    }
}

impl<R> PendingBatch<R> {
    pub fn push(&mut self, scope: &str, path: &str, rows: Vec<R>) {
        self.rows += rows.len();
        self.items.push((scope.to_string(), path.to_string(), rows));
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.items.len() >= BATCH_MAX_DOCS || self.rows >= BATCH_MAX_ROWS
    }

    pub fn take(&mut self) -> Vec<(String, String, Vec<R>)> {
        self.rows = 0;
        std::mem::take(&mut self.items)
    }
}

/// `(scope = 'a' AND key IN ('p1', 'p2')) OR (scope = 'b' AND ...)` for every distinct scope.
fn replace_predicate<R>(key_col: &str, items: &[(String, String, Vec<R>)]) -> String {
    let mut scopes: Vec<(&str, Vec<&str>)> = Vec::new();
    for (scope, path, _) in items {
        match scopes.iter_mut().find(|(s, _)| *s == scope.as_str()) {
            Some((_, paths)) => paths.push(path),
            None => scopes.push((scope, vec![path])),
        }
    }
    scopes
        .iter()
        .map(|(scope, paths)| {
            let list = paths
                .iter()
                .map(|p| format!("'{}'", escape_sql_literal(p)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("(scope = '{}' AND {key_col} IN ({list}))", escape_sql_literal(scope))
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

impl LanceStore {
    /// Replace the rows of every `(scope, path, rows)` item with ONE delete and ONE append (two
    /// commits total, however many documents). An item with no rows just deletes its path.
    pub fn replace_documents_many(&self, items: Vec<(String, String, Vec<DocumentRow>)>) -> Result<()> {
        self.replace_many(DOCUMENTS_TABLE, "path", items, |dim, rows| {
            build_documents_batch(dim, rows)
        })
    }

    /// [`Self::replace_documents_many`] for the `doc_links` table (key column `doc_path`).
    #[cfg(feature = "documents")]
    pub fn replace_doc_links_many(&self, items: Vec<(String, String, Vec<super::DocLinkRow>)>) -> Result<()> {
        self.replace_many(super::schema::DOC_LINKS_TABLE, "doc_path", items, |_, rows| {
            super::doc_links::build_doc_links_batch(rows)
        })
    }

    /// [`Self::replace_documents_many`] for the `code_chunks` table.
    #[cfg(feature = "code-search")]
    pub fn replace_code_chunks_many(&self, items: Vec<(String, String, Vec<super::CodeRow>)>) -> Result<()> {
        self.replace_many(super::schema::CODE_CHUNKS_TABLE, "path", items, |dim, rows| {
            super::build_code_chunks_batch(dim, rows)
        })
    }

    fn replace_many<R>(
        &self,
        table_name: &str,
        key_col: &str,
        items: Vec<(String, String, Vec<R>)>,
        build: impl Fn(u16, &[R]) -> Result<arrow_array::RecordBatch>,
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        self.inner.rt().block_on(async {
            let table = self
                .inner
                .connection
                .open_table(table_name)
                .execute()
                .await
                .with_context(|| format!("open {table_name} table"))?;
            // Skip the delete (a scan plus a commit) when the table holds nothing to replace; this
            // is every batch of a first scan.
            if table
                .count_rows(None)
                .await
                .with_context(|| format!("count {table_name} rows"))?
                > 0
            {
                table
                    .delete(&replace_predicate(key_col, &items))
                    .await
                    .with_context(|| format!("delete {} paths from {table_name}", items.len()))?;
                self.after_commit(&table, table_name).await;
            }
            let flat: Vec<R> = items.into_iter().flat_map(|(_, _, rows)| rows).collect();
            if flat.is_empty() {
                return anyhow::Ok(());
            }
            table
                .add(build(self.inner.dim, &flat)?)
                .execute()
                .await
                .with_context(|| format!("insert {} {table_name} rows", flat.len()))?;
            self.after_commit(&table, table_name).await;
            anyhow::Ok(())
        })
    }

    /// Count a commit against `table_name` and run the bounded-retention maintenance when due.
    /// Best effort: maintenance failures are logged, never propagated into the write.
    pub(super) async fn after_commit(&self, table: &Table, table_name: &str) {
        let n = {
            let mut map = self.inner.commits.lock().unwrap_or_else(|e| e.into_inner());
            let n = map.entry(table_name.to_string()).or_insert(0);
            *n += 1;
            *n
        };
        if n % AUTO_PRUNE_EVERY != 0 {
            return;
        }
        let compact = n % AUTO_COMPACT_EVERY == 0;
        if let Err(error) = optimize_table(table, compact, AUTO_RETENTION).await {
            tracing::warn!(table = table_name, ?error, "lance auto-maintenance failed");
        }
    }

    /// Compact small fragments and prune versions older than `retention` on every table that took
    /// writes since the last call. Cheap when nothing was written. Run at the end of a scan.
    pub fn maintain(&self, retention: Duration) -> Result<()> {
        let dirty: Vec<String> = {
            let mut map = self.inner.commits.lock().unwrap_or_else(|e| e.into_inner());
            let names: Vec<String> = map.iter().filter(|(_, n)| **n > 0).map(|(k, _)| k.clone()).collect();
            map.clear();
            names
        };
        self.inner.rt().block_on(async {
            for name in dirty {
                let table = self
                    .inner
                    .connection
                    .open_table(&name)
                    .execute()
                    .await
                    .with_context(|| format!("open {name} table"))?;
                optimize_table(&table, true, retention)
                    .await
                    .with_context(|| format!("optimize {name}"))?;
            }
            anyhow::Ok(())
        })
    }

    /// Table version counts, for diagnostics and the benchmark: `(table, versions)`.
    pub fn version_counts(&self) -> Result<Vec<(String, usize)>> {
        self.inner.rt().block_on(async {
            let mut out = Vec::new();
            for name in self.inner.connection.table_names().execute().await? {
                let table = self.inner.connection.open_table(&name).execute().await?;
                out.push((name, table.list_versions().await?.len()));
            }
            out.sort();
            anyhow::Ok(out)
        })
    }
}

async fn optimize_table(table: &Table, compact: bool, retention: Duration) -> Result<()> {
    if compact {
        table
            .optimize(OptimizeAction::Compact {
                options: CompactionOptions::default(),
                remap_options: None,
            })
            .await
            .context("compact")?;
    }
    let older_than = ChronoDuration::try_seconds(i64::try_from(retention.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_else(ChronoDuration::zero);
    table
        .optimize(OptimizeAction::Prune {
            older_than: Some(older_than),
            delete_unverified: Some(false),
            error_if_tagged_old_versions: Some(false),
        })
        .await
        .context("prune")?;
    Ok(())
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod tests;
