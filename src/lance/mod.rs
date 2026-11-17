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

mod batch;
#[cfg(feature = "documents")]
pub mod docs;
#[cfg(feature = "documents")]
pub mod docs_page;
#[cfg(feature = "documents")]
pub mod docs_rank;
#[cfg(feature = "documents")]
pub mod docs_schema;
#[cfg(feature = "documents")]
pub mod docs_types;
mod memory;
pub mod schema;
#[doc(hidden)]
pub mod shared;
#[cfg(any(feature = "documents", feature = "code-search"))]
pub(crate) mod shared_snapshot;
#[doc(hidden)]
pub mod shared_store;

pub use batch::{BATCH_MAX_DOCS, BATCH_MAX_ROWS, PendingBatch, SCAN_RETENTION};
pub use memory::MemoryStamps;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use futures::TryStreamExt;
use lancedb::Connection;
use lancedb::arrow::arrow_array::builder::{FixedSizeListBuilder, Float32Builder, StringBuilder, UInt32Builder};
use lancedb::arrow::arrow_array::{Array, RecordBatch, StringArray};
use lancedb::query::{ExecutableQuery, QueryBase};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;

use schema::{DOCUMENTS_TABLE, MEMORY_TABLE, documents_schema, memory_schema};

/// On-disk metadata for the lance store. Tracks the vector dim, the
/// embedding-model identifier, and the Arrow table schema version; a mismatch on
/// any field wipes the store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct LanceMeta {
    dim: u16,
    embedding_model: String,
    /// Arrow schema version of the `memory` / `documents` tables (see
    /// [`VECTOR_STORE_VER`]). `#[serde(default)]` makes a pre-0.5 `meta.json` (which
    /// lacks this field) deserialize as `0`, forcing a wipe on upgrade rather than a
    /// parse error — the table column set changed, and a stale-arity table would fault
    /// at batch-build time, not at open.
    #[serde(default)]
    schema_ver: u32,
}

const META_FILE: &str = "meta.json";

/// File beside `meta.json` naming this store: written when the store is created, gone with it when
/// it is wiped or removed. A scan records the id of the store it wrote a view's rows to, so a
/// store that was wiped or deleted since is told from the one that holds those rows.
const STORE_ID_FILE: &str = "store-id";

/// Lock file inside the store directory, held while an open decides whether to wipe the store and
/// creates what is missing. Without it one process can remove, as "tables beside no meta.json",
/// the tables another process created a moment after writing its own.
const OPEN_LOCK_FILE: &str = ".open.lock";

/// The id of the store at `dir`, or `None` when there is no store there or it carries none.
pub fn store_id(dir: &Path) -> Option<String> {
    let id = std::fs::read_to_string(dir.join(STORE_ID_FILE)).ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Give the store at `dir` an id when it has none: a new store, or one an earlier build created.
fn ensure_store_id(dir: &Path) -> Result<()> {
    if store_id(dir).is_some() {
        return Ok(());
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let path = dir.join(STORE_ID_FILE);
    std::fs::write(&path, format!("{nanos:x}-{:x}", std::process::id()))
        .with_context(|| format!("write {}", path.display()))
}

/// How long an open waits for another open of the same store to finish. An open holds the lock
/// only while it creates the store's files, so a holder past this is stuck, not busy.
const OPEN_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Hold the open lock of the store at `dir`, waiting at most `wait` for it. `Ok(None)` when the lock
/// file cannot be created: the open goes on unserialised, as it would on a directory it cannot
/// write. An error when another open holds the lock past `wait`: that open may be wiping the
/// store, and opening beside it would race the wipe, so this open fails and the next search or
/// scan tries again.
fn lock_open(dir: &Path, wait: std::time::Duration) -> Result<Option<crate::store_lock::LockGuard>> {
    const RETRY: std::time::Duration = std::time::Duration::from_millis(20);
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(OPEN_LOCK_FILE))
    else {
        return Ok(None);
    };
    let deadline = std::time::Instant::now() + wait;
    loop {
        if fs2::FileExt::try_lock_exclusive(&file).is_ok() {
            return Ok(Some(crate::store_lock::LockGuard::held(file)));
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "another open of the vector store at {} held its lock for over {} s; not opening beside it",
                dir.display(),
                wait.as_secs_f32()
            ));
        }
        std::thread::sleep(RETRY);
    }
}

/// Table-layout version of every LanceDB store (the memory vectors, the workspace vector store and
/// the document retrieval store), recorded in `meta.json` beside the model and dimension.
///
/// Independent of the release minor (ADR-0035): a minor that changes no table leaves the stores,
/// and the memory vectors in them, in place. Stale rows are kept out by the keys, not by a wipe: a
/// store directory is named by its model and dimension, a document's `doc_key` hashes the blob
/// schema, and a memory row is checked against its note's `updated_at`. Bump it when a table's
/// columns change; an older store is then wiped and rebuilt, a newer one refused. It starts at
/// 1000 so every store a minor-numbered binary wrote (`schema_ver` = the minor) reads as older.
pub const VECTOR_STORE_VER: u32 = 1000;

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

/// One row in the `memory` table. It carries the note's vector and tags, never its text: a row
/// can outlive its note, and the text of a deleted note must not.
#[derive(Debug, Clone)]
pub struct MemoryRow {
    pub scope: String,
    pub key: String,
    pub tags: Vec<String>,
    /// Memory tier: `"group"` (shared) or `"individual"` (per-agent).
    pub visibility: String,
    /// Owner of an individual-tier row; empty string for the group tier.
    pub agent_id: String,
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

/// A search hit from the `memory` table: which note, and which write of it. The note's text and
/// tags are read from the note itself.
#[derive(Debug, Clone)]
pub struct MemoryHit {
    pub key: String,
    /// The `updated_at` of the note this row was embedded from. A row whose stamp differs from the
    /// note's own was left behind by a newer write and must not be served.
    pub updated_at: i64,
    pub distance: f32,
}

/// One row in the `code_chunks` table.
#[cfg(feature = "code-search")]
#[derive(Debug, Clone)]
pub struct CodeRow {
    pub scope: String,
    pub path: String,
    pub chunk_id: String,
    pub symbol: String,
    pub kind: String,
    pub lang: String,
    pub line_start: u32,
    pub line_end: u32,
    pub byte_start: u32,
    pub byte_end: u32,
    pub text: String,
    pub embedding: Vec<f32>,
}

/// A search hit from the `code_chunks` table — a pointer, not a body.
#[cfg(feature = "code-search")]
#[derive(Debug, Clone)]
pub struct CodeChunkHit {
    pub path: String,
    pub chunk_id: String,
    pub symbol: String,
    pub kind: String,
    pub lang: String,
    pub line_start: u32,
    pub line_end: u32,
    pub byte_start: u32,
    pub byte_end: u32,
    /// L2 distance from the query vector (lower = closer).
    pub distance: f32,
}

/// Embedded LanceDB store. Cheap to clone (internal Arc).
#[derive(Clone)]
pub struct LanceStore {
    inner: Arc<LanceStoreInner>,
}

struct LanceStoreInner {
    /// `Some` for the whole lifetime; taken in `Drop` so the runtime can be torn
    /// down off the current async context (see the `Drop` impl).
    runtime: Option<Runtime>,
    connection: Connection,
    dim: u16,
    embedding_model: String,
    dir: PathBuf,
    /// Commits per table since the last maintenance pass; drives bounded version retention.
    commits: std::sync::Mutex<std::collections::HashMap<String, u64>>,
}

impl LanceStoreInner {
    /// The owned tokio runtime. Present until `Drop` takes it.
    fn rt(&self) -> &Runtime {
        self.runtime.as_ref().expect("LanceStore runtime is present until drop")
    }
}

impl Drop for LanceStoreInner {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl LanceStore {
    /// Open (or initialise) the lance store rooted at `dir`. If a pre-existing
    /// meta.json reports a different `(dim, embedding_model, schema_ver)` triple, the
    /// entire dir is wiped and rebuilt before the connection opens — unless its `schema_ver`
    /// is newer than this binary's, which fails the open and leaves the dir untouched. A dir
    /// with tables and no meta.json is emptied too.
    pub fn open(dir: &Path, dim: u16, embedding_model: &str) -> Result<Self> {
        Self::open_waiting(dir, dim, embedding_model, OPEN_LOCK_WAIT)
    }

    /// [`Self::open`] waiting at most `wait` for another open of the store.
    fn open_waiting(dir: &Path, dim: u16, embedding_model: &str, wait: std::time::Duration) -> Result<Self> {
        let tables = [
            (DOCUMENTS_TABLE, documents_schema(dim)),
            (MEMORY_TABLE, memory_schema(dim)),
            #[cfg(feature = "code-search")]
            (schema::CODE_CHUNKS_TABLE, schema::code_chunks_schema(dim)),
        ];
        Self::open_with(dir, dim, embedding_model, wait, &tables)
    }

    /// Open the store at `dir` holding exactly `tables`, under the same version, wipe and lock
    /// rules as [`Self::open`].
    #[cfg(feature = "documents")]
    pub(crate) fn open_tables(
        dir: &Path,
        dim: u16,
        embedding_model: &str,
        tables: &[(&str, lancedb::arrow::arrow_schema::SchemaRef)],
    ) -> Result<Self> {
        Self::open_with(dir, dim, embedding_model, OPEN_LOCK_WAIT, tables)
    }

    fn open_with(
        dir: &Path,
        dim: u16,
        embedding_model: &str,
        wait: std::time::Duration,
        tables: &[(&str, lancedb::arrow::arrow_schema::SchemaRef)],
    ) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let meta_path = dir.join(META_FILE);
        let expected = LanceMeta {
            dim,
            embedding_model: crate::embeddings::model_key(embedding_model).into_owned(),
            schema_ver: VECTOR_STORE_VER,
        };
        // Released when this function returns, tables created.
        let _opening = lock_open(dir, wait)?;
        wipe_on_mismatch(dir, &meta_path, &expected)?;
        // meta.json goes in before any table. Tables beside no meta.json have no known model: they
        // are what an open that died before its meta write left, and are removed, not adopted.
        if !meta_path.exists() {
            remove_entries(dir, &meta_path, &remove_entry)?;
            write_meta(&meta_path, &expected)?;
        }
        ensure_store_id(dir)?;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build tokio runtime for LanceStore")?;
        let uri = dir.to_string_lossy().into_owned();
        let connection = runtime
            .block_on(async { lancedb::connect(&uri).execute().await })
            .with_context(|| format!("open lancedb at {uri}"))?;

        runtime.block_on(async {
            for (name, schema) in tables {
                ensure_table(&connection, name, schema.clone()).await?;
            }
            anyhow::Ok(())
        })?;

        Ok(Self {
            inner: Arc::new(LanceStoreInner {
                runtime: Some(runtime),
                connection,
                dim,
                embedding_model: expected.embedding_model.clone(),
                dir: dir.to_path_buf(),
                commits: std::sync::Mutex::new(std::collections::HashMap::new()),
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
        self.inner.rt().block_on(async {
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
            self.after_commit(&table, DOCUMENTS_TABLE).await;
            if rows.is_empty() {
                return Ok(());
            }
            let batch = build_documents_batch(self.inner.dim, &rows)?;
            table
                .add(batch)
                .execute()
                .await
                .with_context(|| format!("insert {} documents rows", rows.len()))?;
            self.after_commit(&table, DOCUMENTS_TABLE).await;
            anyhow::Ok(())
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
        self.inner.rt().block_on(async {
            let table = self
                .inner
                .connection
                .open_table(DOCUMENTS_TABLE)
                .execute()
                .await
                .with_context(|| format!("open {DOCUMENTS_TABLE} table"))?;
            let mut q = table.vector_search(query).context("build vector search")?.limit(limit);
            let scope_clause = format!("scope = '{}'", escape_sql_literal(scope));
            q = match mime_type_filter {
                Some(m) => q.only_if(format!("{scope_clause} AND mime_type = '{}'", escape_sql_literal(m))),
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

    /// Replace all `code_chunks` rows for a `(scope, path)` pair and insert the supplied rows.
    /// Delete-then-insert mirrors [`Self::replace_document`] so a re-scan never leaves stale
    /// chunks behind.
    #[cfg(feature = "code-search")]
    pub fn replace_code_chunks(&self, scope: &str, path: &str, rows: Vec<CodeRow>) -> Result<()> {
        self.inner.rt().block_on(async {
            let table = self
                .inner
                .connection
                .open_table(schema::CODE_CHUNKS_TABLE)
                .execute()
                .await
                .with_context(|| format!("open {} table", schema::CODE_CHUNKS_TABLE))?;
            let predicate = format!(
                "scope = '{}' AND path = '{}'",
                escape_sql_literal(scope),
                escape_sql_literal(path)
            );
            table
                .delete(&predicate)
                .await
                .with_context(|| format!("delete existing code chunks for {scope}/{path}"))?;
            self.after_commit(&table, schema::CODE_CHUNKS_TABLE).await;
            if rows.is_empty() {
                return Ok(());
            }
            let batch = build_code_chunks_batch(self.inner.dim, &rows)?;
            table
                .add(batch)
                .execute()
                .await
                .with_context(|| format!("insert {} code_chunks rows", rows.len()))?;
            self.after_commit(&table, schema::CODE_CHUNKS_TABLE).await;
            anyhow::Ok(())
        })
    }

    /// Delete every `code_chunks` row for a `(scope, path)` pair without inserting replacements.
    /// The scanner's stale-file pass calls this when a file is removed, so a deleted path leaves no
    /// dangling `search_code` hits. Idempotent — deleting a path with no rows is a no-op.
    #[cfg(feature = "code-search")]
    pub fn delete_code_chunks(&self, scope: &str, path: &str) -> Result<()> {
        self.inner.rt().block_on(async {
            let table = self
                .inner
                .connection
                .open_table(schema::CODE_CHUNKS_TABLE)
                .execute()
                .await
                .with_context(|| format!("open {} table", schema::CODE_CHUNKS_TABLE))?;
            let predicate = format!(
                "scope = '{}' AND path = '{}'",
                escape_sql_literal(scope),
                escape_sql_literal(path)
            );
            table
                .delete(&predicate)
                .await
                .with_context(|| format!("delete code chunks for {scope}/{path}"))?;
            anyhow::Ok(())
        })
    }

    /// Delete every `code_chunks` row of `paths` in `scope`, batching the paths into `IN (...)`
    /// predicates so a purge of N files is a handful of deletes rather than N table versions.
    #[cfg(feature = "code-search")]
    pub fn delete_code_chunks_for_paths(&self, scope: &str, paths: &[String]) -> Result<()> {
        self.delete_paths_in(schema::CODE_CHUNKS_TABLE, "path", scope, paths)
    }

    /// Delete every `documents` row of `paths` in `scope`; the batched counterpart of
    /// [`Self::replace_document`] with no replacement rows.
    pub fn delete_documents_for_paths(&self, scope: &str, paths: &[String]) -> Result<()> {
        self.delete_paths_in(DOCUMENTS_TABLE, "path", scope, paths)
    }

    pub(super) fn delete_paths_in(&self, table_name: &str, key_col: &str, scope: &str, paths: &[String]) -> Result<()> {
        // Each batch is one delete transaction (a new table version plus a scan of `path`), so a
        // large purge is dominated by the batch COUNT; 2000 literals keep the predicate small.
        const BATCH: usize = 2_000;
        if paths.is_empty() {
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
            // Nothing stored for this scope (the common case on the first scan after an upgrade, or
            // with embeddings off): skip the N/200 deletes, each of which scans the table and may
            // commit a table version.
            let in_scope = table
                .count_rows(Some(format!("scope = '{}'", escape_sql_literal(scope))))
                .await
                .with_context(|| format!("count {table_name} rows in {scope}"))?;
            if in_scope == 0 {
                return anyhow::Ok(());
            }
            for batch in paths.chunks(BATCH) {
                let list = batch
                    .iter()
                    .map(|p| format!("'{}'", escape_sql_literal(p)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let predicate = format!("scope = '{}' AND {key_col} IN ({list})", escape_sql_literal(scope));
                table
                    .delete(&predicate)
                    .await
                    .with_context(|| format!("delete {} paths from {table_name} in {scope}", batch.len()))?;
                self.after_commit(&table, table_name).await;
            }
            anyhow::Ok(())
        })
    }

    /// KNN over the `code_chunks` table for one scope. Returns pointer hits (path + span +
    /// symbol + distance), best-first.
    #[cfg(feature = "code-search")]
    pub fn search_code_chunks(&self, scope: &str, query: Vec<f32>, limit: usize) -> Result<Vec<CodeChunkHit>> {
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
                .open_table(schema::CODE_CHUNKS_TABLE)
                .execute()
                .await
                .with_context(|| format!("open {} table", schema::CODE_CHUNKS_TABLE))?;
            let q = table
                .vector_search(query)
                .context("build code chunk vector search")?
                .limit(limit)
                .only_if(format!("scope = '{}'", escape_sql_literal(scope)));
            let mut stream = q.execute().await.context("run code chunk search")?;
            let mut hits = Vec::new();
            while let Some(batch) = stream.try_next().await.context("stream next batch")? {
                decode_code_chunk_hits(&batch, &mut hits)?;
            }
            anyhow::Ok(hits)
        })
    }
}

/// Write `meta.json` through a sibling temp file and a rename, so a crash leaves the previous file
/// or the whole new one, never a truncated file that fails every later open.
fn write_meta(meta_path: &Path, meta: &LanceMeta) -> Result<()> {
    use std::io::Write;

    let body = serde_json::to_vec_pretty(meta).context("serialize lance meta.json")?;
    // Named by pid: two processes opening a new store at once must not write one temp file.
    let tmp = meta_path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut file = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    file.write_all(&body)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, meta_path).with_context(|| format!("write {}", meta_path.display()))
}

/// The one field of `meta.json` every layout of it has had. Read on its own, so a file whose other
/// fields this binary cannot parse still says which basemind wrote it.
#[derive(Deserialize)]
struct MetaVersion {
    schema_ver: u32,
}

fn wipe_on_mismatch(dir: &Path, meta_path: &Path, expected: &LanceMeta) -> Result<()> {
    if !meta_path.exists() {
        return Ok(());
    }
    let bytes = std::fs::read(meta_path).with_context(|| format!("read {}", meta_path.display()))?;
    // A newer store is refused before anything else is asked of its meta.json: a newer layout
    // need not parse as this one, and a parse failure must not read as leave to wipe it.
    if let Ok(MetaVersion { schema_ver }) = serde_json::from_slice(&bytes)
        && schema_ver > expected.schema_ver
    {
        return Err(anyhow!(
            "lance store at {} written by a newer basemind (schema {schema_ver}); this binary understands {} — upgrade basemind",
            dir.display(),
            expected.schema_ver
        ));
    }
    // A meta.json with no readable version says nothing about the tables beside it, which is a
    // mismatch, not a reason to fail every open of a store that is only a cache.
    match serde_json::from_slice::<LanceMeta>(&bytes) {
        Ok(actual) if actual == *expected => return Ok(()),
        Ok(actual) => tracing::warn!(
            old_dim = actual.dim,
            new_dim = expected.dim,
            old_model = %actual.embedding_model,
            new_model = %expected.embedding_model,
            old_schema_ver = actual.schema_ver,
            new_schema_ver = expected.schema_ver,
            "lance store dim/model/schema mismatch — wiping {}",
            dir.display()
        ),
        Err(error) => tracing::warn!(%error, "lance meta.json is unreadable — wiping {}", dir.display()),
    }
    // meta.json goes last: while it is there a wipe that was cut short is seen and finished by the
    // next open.
    remove_entries(dir, meta_path, &remove_entry)?;
    std::fs::remove_file(meta_path).with_context(|| format!("remove {}", meta_path.display()))
}

/// Remove every entry of `dir` but `keep` and the open lock, through `remove`.
fn remove_entries(dir: &Path, keep: &Path, remove: &dyn Fn(&Path) -> std::io::Result<()>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let path = entry.context("entry")?.path();
        if path != keep && path.file_name().is_none_or(|name| name != OPEN_LOCK_FILE) {
            remove(&path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    Ok(())
}

fn remove_entry(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

async fn ensure_table(
    connection: &Connection,
    name: &str,
    schema: lancedb::arrow::arrow_schema::SchemaRef,
) -> Result<()> {
    let existing: Vec<String> = connection.table_names().execute().await.context("list lance tables")?;
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

#[cfg(feature = "code-search")]
fn build_code_chunks_batch(dim: u16, rows: &[CodeRow]) -> Result<RecordBatch> {
    let mut scope = StringBuilder::new();
    let mut path = StringBuilder::new();
    let mut chunk_id = StringBuilder::new();
    let mut symbol = StringBuilder::new();
    let mut kind = StringBuilder::new();
    let mut lang = StringBuilder::new();
    let mut line_start = UInt32Builder::new();
    let mut line_end = UInt32Builder::new();
    let mut byte_start = UInt32Builder::new();
    let mut byte_end = UInt32Builder::new();
    let mut text = StringBuilder::new();
    let mut embedding = FixedSizeListBuilder::new(Float32Builder::new(), i32::from(dim));

    for r in rows {
        if r.embedding.len() != usize::from(dim) {
            return Err(anyhow!(
                "code_chunks row embedding dim {} does not match store dim {}",
                r.embedding.len(),
                dim
            ));
        }
        scope.append_value(&r.scope);
        path.append_value(&r.path);
        chunk_id.append_value(&r.chunk_id);
        symbol.append_value(&r.symbol);
        kind.append_value(&r.kind);
        lang.append_value(&r.lang);
        line_start.append_value(r.line_start);
        line_end.append_value(r.line_end);
        byte_start.append_value(r.byte_start);
        byte_end.append_value(r.byte_end);
        text.append_value(&r.text);
        for v in &r.embedding {
            embedding.values().append_value(*v);
        }
        embedding.append(true);
    }

    let schema = schema::code_chunks_schema(dim);
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(scope.finish()),
            Arc::new(path.finish()),
            Arc::new(chunk_id.finish()),
            Arc::new(symbol.finish()),
            Arc::new(kind.finish()),
            Arc::new(lang.finish()),
            Arc::new(line_start.finish()),
            Arc::new(line_end.finish()),
            Arc::new(byte_start.finish()),
            Arc::new(byte_end.finish()),
            Arc::new(text.finish()),
            Arc::new(embedding.finish()),
        ],
    )
    .context("assemble code_chunks batch")
}

#[cfg(feature = "code-search")]
fn decode_code_chunk_hits(batch: &RecordBatch, out: &mut Vec<CodeChunkHit>) -> Result<()> {
    decode_code_chunk_hits_with_path_column(batch, "path", out)
}

#[cfg(feature = "code-search")]
fn decode_code_chunk_hits_with_path_column(
    batch: &RecordBatch,
    path_column: &str,
    out: &mut Vec<CodeChunkHit>,
) -> Result<()> {
    use lancedb::arrow::arrow_array::{Float32Array, UInt32Array};
    let str_col = |name: &str| -> Result<&StringArray> {
        batch
            .column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .ok_or_else(|| anyhow!("`{name}` column missing"))
    };
    let u32_col = |name: &str| -> Result<&UInt32Array> {
        batch
            .column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref::<UInt32Array>())
            .ok_or_else(|| anyhow!("`{name}` column missing"))
    };
    let path = str_col(path_column)?;
    let chunk_id = str_col("chunk_id")?;
    let symbol = str_col("symbol")?;
    let kind = str_col("kind")?;
    let lang = str_col("lang")?;
    let line_start = u32_col("line_start")?;
    let line_end = u32_col("line_end")?;
    let byte_start = u32_col("byte_start")?;
    let byte_end = u32_col("byte_end")?;
    let distance = batch
        .column_by_name("_distance")
        .and_then(|c| c.as_any().downcast_ref::<Float32Array>());

    for i in 0..batch.num_rows() {
        out.push(CodeChunkHit {
            path: path.value(i).to_string(),
            chunk_id: chunk_id.value(i).to_string(),
            symbol: symbol.value(i).to_string(),
            kind: kind.value(i).to_string(),
            lang: lang.value(i).to_string(),
            line_start: line_start.value(i),
            line_end: line_end.value(i),
            byte_start: byte_start.value(i),
            byte_end: byte_end.value(i),
            distance: distance.map(|d| d.value(i)).unwrap_or(0.0),
        });
    }
    Ok(())
}

fn decode_document_hits(batch: &RecordBatch, out: &mut Vec<DocumentHit>) -> Result<()> {
    decode_document_hits_with_path_column(batch, "path", out)
}

fn decode_document_hits_with_path_column(
    batch: &RecordBatch,
    path_column: &str,
    out: &mut Vec<DocumentHit>,
) -> Result<()> {
    use lancedb::arrow::arrow_array::{Float32Array, UInt32Array};
    let path = batch
        .column_by_name(path_column)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| anyhow!("`{path_column}` column missing"))?;
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

/// Single-quote escape for the simple SQL-literal predicates we use.
fn escape_sql_literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// Predicate selecting a whole `(scope, visibility, agent_id)` memory namespace.
///
/// Used by [`LanceStore::search_memory`] so an individual search can never surface another
/// agent's rows and a group search only sees group rows.
fn memory_namespace_predicate(scope: &str, visibility: &str, agent_id: &str) -> String {
    format!(
        "scope = '{}' AND visibility = '{}' AND agent_id = '{}'",
        escape_sql_literal(scope),
        escape_sql_literal(visibility),
        escape_sql_literal(agent_id),
    )
}

/// Convenience: current time as microseconds since unix epoch, saturating on
/// the (effectively impossible) clock-before-epoch case.
pub fn now_micros() -> i64 {
    let dur = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    i64::try_from(dur.as_micros()).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
