use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::extract::{FileMapL1, FileMapL2, SCHEMA_VER};
use crate::hashing::{self, Hash};
use crate::index::{IndexDb, IndexError};
#[cfg(feature = "intelligence")]
use crate::lance::LanceStore;
use crate::path::RelPath;

pub const INDEX_FILE: &str = "index.msgpack";
pub const BLOBS_DIR: &str = "blobs";
pub const LOCK_FILE: &str = ".lock";
pub const VIEWS_DIR: &str = "views";
/// Lazy-opened LanceDB store directory under `.basemind/`. Created on first use.
#[cfg(feature = "intelligence")]
pub const LANCE_DIR: &str = "lance";

/// View name used for the working-tree index. Also the default for `basemind serve`.
pub const VIEW_WORKING: &str = "working";
/// View name used when scanning the staging index.
pub const VIEW_STAGED: &str = "staged";

/// Build the view name used for an arbitrary rev. Slash-free so it's a single directory.
pub fn view_name_for_rev(short_sha: &str) -> String {
    format!("rev-{short_sha}")
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("msgpack encode error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("msgpack decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("schema version mismatch: stored {found}, current {expected}")]
    SchemaMismatch { found: u16, expected: u16 },
    #[error("another basemind process holds the lock on {0} (likely `basemind watch` is running)")]
    Locked(PathBuf),
    #[error("inverted index error: {0}")]
    Index(#[from] IndexError),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Index {
    pub schema_ver: u16,
    /// Relative path → FileEntry. Keyed by `RelPath` so paths with non-UTF-8 bytes
    /// round-trip losslessly through the msgpack store; valid UTF-8 paths serialize as
    /// plain strings (zero wire-format churn for the common case).
    pub files: BTreeMap<RelPath, FileEntry>,
}

impl Index {
    pub fn empty() -> Self {
        Self {
            schema_ver: SCHEMA_VER,
            files: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub hash_hex: String,
    pub language: String,
    pub size_bytes: u64,
    /// File mtime in seconds since the epoch. Cheap pre-filter before hashing.
    pub mtime: i64,
}

pub struct Store {
    pub root: PathBuf,
    pub basemind_dir: PathBuf,
    pub view_dir: PathBuf,
    pub view: String,
    pub index: Index,
    /// Fjall-backed inverted index over symbols / calls / imports. Lives under
    /// `view_dir/index.fjall/`. Reads + writes from any caller go through `IndexDb`,
    /// which is cheap to clone (internally Arc'd). `None` in read-only mode when the
    /// directory doesn't exist yet — callers must handle the absence.
    pub index_db: Option<IndexDb>,
    /// LanceDB-backed vector store. Lazy-opened on first document insert so a
    /// vanilla code-only scan doesn't pay the LanceDB startup cost.
    #[cfg(feature = "intelligence")]
    pub lance: Option<LanceStore>,
    _lock: Option<File>,
}

impl Store {
    /// Open the store for a specific view. View names are flat strings: `"working"`,
    /// `"staged"`, `"rev-<sha7>"`. Each view has its own `index.msgpack` under
    /// `.basemind/views/<view>/`; blobs are shared in `.basemind/blobs/`.
    pub fn open(root: &Path, view: &str) -> Result<Self, StoreError> {
        let basemind_dir = root.join(crate::config::BASEMIND_DIR);
        ensure_dir(&basemind_dir)?;
        ensure_dir(&basemind_dir.join(BLOBS_DIR))?;
        ensure_dir(&basemind_dir.join(VIEWS_DIR))?;
        migrate_legacy_index_into_views(&basemind_dir)?;

        let view_dir = basemind_dir.join(VIEWS_DIR).join(view);
        ensure_dir(&view_dir)?;
        let lock = acquire_lock(&basemind_dir)?;
        let index = match read_index(&view_dir) {
            Ok(Some(idx)) => idx,
            Ok(None) => Index::empty(),
            Err(StoreError::SchemaMismatch { found, expected }) => {
                tracing::info!(
                    found,
                    expected,
                    view,
                    "cache schema bumped; wiping view index + shared blobs"
                );
                wipe_view(&view_dir)?;
                wipe_blobs(&basemind_dir)?;
                Index::empty()
            }
            Err(e) => return Err(e),
        };
        let index_db = Some(IndexDb::open(&view_dir)?);
        Ok(Self {
            root: root.to_path_buf(),
            basemind_dir,
            view_dir,
            view: view.to_string(),
            index,
            index_db,
            #[cfg(feature = "intelligence")]
            lance: None,
            _lock: Some(lock),
        })
    }

    /// Open without taking the exclusive lock. Use for read-only consumers (CLI query, MCP).
    pub fn open_read_only(root: &Path, view: &str) -> Result<Self, StoreError> {
        let basemind_dir = root.join(crate::config::BASEMIND_DIR);
        // Idempotent migration so a read-only consumer sees the same shape a fresh writer would.
        if basemind_dir.exists() {
            let _ = migrate_legacy_index_into_views(&basemind_dir);
        }
        let view_dir = basemind_dir.join(VIEWS_DIR).join(view);
        let index = read_index(&view_dir)?.unwrap_or_else(Index::empty);
        // The MCP server runs read-only; we still need to *read* from the Fjall index for
        // find_references etc. Opening it here is harmless — Fjall handles concurrent
        // readers fine via internal snapshots.
        let index_db = if view_dir.exists() {
            IndexDb::open(&view_dir).ok()
        } else {
            None
        };
        Ok(Self {
            root: root.to_path_buf(),
            basemind_dir,
            view_dir,
            view: view.to_string(),
            index,
            index_db,
            #[cfg(feature = "intelligence")]
            lance: None,
            _lock: None,
        })
    }

    /// Lazy-open the LanceDB store at `.basemind/lance/`. Subsequent calls return
    /// the cached handle; the first call pays the connection + table-init cost.
    ///
    /// A mismatch between the stored `(dim, embedding_model)` and the values
    /// passed here wipes the whole `.basemind/lance/` directory and rebuilds —
    /// the standard schema-bump migration story for the vector store.
    #[cfg(feature = "intelligence")]
    pub fn lance_or_open(
        &mut self,
        dim: u16,
        embedding_model: &str,
    ) -> Result<&LanceStore, anyhow::Error> {
        if self.lance.is_none() {
            let dir = self.basemind_dir.join(LANCE_DIR);
            let store = LanceStore::open(&dir, dim, embedding_model)?;
            self.lance = Some(store);
        }
        // SAFETY of unwrap: we just populated it on the line above when None.
        Ok(self.lance.as_ref().expect("lance store just populated"))
    }

    pub fn blob_path_l1(&self, hash: &Hash) -> PathBuf {
        let buf = hashing::hex_buf(hash);
        self.blob_path_l1_hex(hashing::hex_str(&buf))
    }

    pub fn blob_path_l2(&self, hash: &Hash) -> PathBuf {
        let buf = hashing::hex_buf(hash);
        self.blob_path_l2_hex(hashing::hex_str(&buf))
    }

    /// Build the L1 blob path from an already-hex-encoded hash. Skips the encode round-trip
    /// when the caller starts from a `FileEntry::hash_hex`.
    pub fn blob_path_l1_hex(&self, hash_hex: &str) -> PathBuf {
        self.basemind_dir
            .join(BLOBS_DIR)
            .join(format!("{hash_hex}.l1.msgpack"))
    }

    pub fn blob_path_l2_hex(&self, hash_hex: &str) -> PathBuf {
        self.basemind_dir
            .join(BLOBS_DIR)
            .join(format!("{hash_hex}.l2.msgpack"))
    }

    #[cfg(feature = "documents")]
    pub fn blob_path_doc(&self, hash: &Hash) -> PathBuf {
        let buf = hashing::hex_buf(hash);
        self.blob_path_doc_hex(hashing::hex_str(&buf))
    }

    #[cfg(feature = "documents")]
    pub fn blob_path_doc_hex(&self, hash_hex: &str) -> PathBuf {
        self.basemind_dir
            .join(BLOBS_DIR)
            .join(format!("{hash_hex}.doc.msgpack"))
    }

    pub fn read_l1(&self, hash: &Hash) -> Result<Option<FileMapL1>, StoreError> {
        let buf = hashing::hex_buf(hash);
        self.read_l1_by_hex(hashing::hex_str(&buf))
    }

    pub fn read_l2(&self, hash: &Hash) -> Result<Option<FileMapL2>, StoreError> {
        let buf = hashing::hex_buf(hash);
        self.read_l2_by_hex(hashing::hex_str(&buf))
    }

    /// Read an L1 blob given its already-hex-encoded hash. Avoids the
    /// `hex → [u8;32] → hex` decode-encode cycle that the `read_l1(&Hash)` path
    /// goes through when callers (CLI query, MCP) already hold the hex string.
    pub fn read_l1_by_hex(&self, hash_hex: &str) -> Result<Option<FileMapL1>, StoreError> {
        let path = self.blob_path_l1_hex(hash_hex);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path).map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
        let map: FileMapL1 = rmp_serde::from_slice(&bytes)?;
        check_schema(map.schema_ver)?;
        Ok(Some(map))
    }

    pub fn read_l2_by_hex(&self, hash_hex: &str) -> Result<Option<FileMapL2>, StoreError> {
        let path = self.blob_path_l2_hex(hash_hex);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path).map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
        let map: FileMapL2 = rmp_serde::from_slice(&bytes)?;
        check_schema(map.schema_ver)?;
        Ok(Some(map))
    }

    pub fn write_l1(&self, hash: &Hash, map: &FileMapL1) -> Result<(), StoreError> {
        write_blob(self.blob_path_l1(hash), map)
    }

    pub fn write_l2(&self, hash: &Hash, map: &FileMapL2) -> Result<(), StoreError> {
        write_blob(self.blob_path_l2(hash), map)
    }

    #[cfg(feature = "documents")]
    pub fn write_doc(
        &self,
        hash: &Hash,
        map: &crate::extract::doc::FileMapDoc,
    ) -> Result<(), StoreError> {
        write_blob(self.blob_path_doc(hash), map)
    }

    #[cfg(feature = "documents")]
    pub fn read_doc_by_hex(
        &self,
        hash_hex: &str,
    ) -> Result<Option<crate::extract::doc::FileMapDoc>, StoreError> {
        let path = self.blob_path_doc_hex(hash_hex);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path).map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
        let map: crate::extract::doc::FileMapDoc = rmp_serde::from_slice(&bytes)?;
        check_schema(map.schema_ver)?;
        Ok(Some(map))
    }

    pub fn upsert(&mut self, rel: impl Into<RelPath>, entry: FileEntry) {
        self.index.files.insert(rel.into(), entry);
    }

    pub fn remove(&mut self, rel: impl AsRef<[u8]>) {
        self.index.files.remove(bstr::BStr::new(rel.as_ref()));
    }

    /// Look a file up by its repository-relative path. Accepts any byte source —
    /// `&str`, `&RelPath`, `&[u8]` — so call sites that already hold a String can keep
    /// working without an explicit conversion.
    pub fn lookup(&self, rel: impl AsRef<[u8]>) -> Option<&FileEntry> {
        self.index.files.get(bstr::BStr::new(rel.as_ref()))
    }

    /// Atomically rewrite the index file (tmp + rename).
    pub fn flush(&self) -> Result<(), StoreError> {
        let final_path = self.view_dir.join(INDEX_FILE);
        let tmp_path = self.view_dir.join(format!("{INDEX_FILE}.tmp"));
        let bytes = rmp_serde::to_vec_named(&self.index)?;
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)
                .map_err(|source| StoreError::Io {
                    path: tmp_path.clone(),
                    source,
                })?;
            f.write_all(&bytes).map_err(|source| StoreError::Io {
                path: tmp_path.clone(),
                source,
            })?;
            f.sync_all().map_err(|source| StoreError::Io {
                path: tmp_path.clone(),
                source,
            })?;
        }
        std::fs::rename(&tmp_path, &final_path).map_err(|source| StoreError::Io {
            path: final_path,
            source,
        })?;
        Ok(())
    }
}

fn ensure_dir(p: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(p).map_err(|source| StoreError::Io {
        path: p.to_path_buf(),
        source,
    })
}

/// Delete the index file in a single view's directory.
fn wipe_view(view_dir: &Path) -> Result<(), StoreError> {
    let index_path = view_dir.join(INDEX_FILE);
    if index_path.exists() {
        std::fs::remove_file(&index_path).map_err(|source| StoreError::Io {
            path: index_path,
            source,
        })?;
    }
    Ok(())
}

/// Wipe the shared blobs directory. Called from `Store::open` when the persisted schema
/// version doesn't match `SCHEMA_VER` — once one view's blobs are stale, all are.
fn wipe_blobs(basemind_dir: &Path) -> Result<(), StoreError> {
    let blobs_dir = basemind_dir.join(BLOBS_DIR);
    if blobs_dir.exists() {
        std::fs::remove_dir_all(&blobs_dir).map_err(|source| StoreError::Io {
            path: blobs_dir.clone(),
            source,
        })?;
        std::fs::create_dir_all(&blobs_dir).map_err(|source| StoreError::Io {
            path: blobs_dir,
            source,
        })?;
    }
    Ok(())
}

/// Pre-views installs kept `index.msgpack` at the top of `.basemind/`. After the upgrade,
/// each view lives under `.basemind/views/<view>/`. If we detect the legacy file AND no
/// working-view file exists yet, move it in place. Idempotent: re-runs are no-ops.
fn migrate_legacy_index_into_views(basemind_dir: &Path) -> Result<(), StoreError> {
    let legacy = basemind_dir.join(INDEX_FILE);
    if !legacy.exists() {
        return Ok(());
    }
    let working_dir = basemind_dir.join(VIEWS_DIR).join(VIEW_WORKING);
    let working_index = working_dir.join(INDEX_FILE);
    if working_index.exists() {
        // Both present — `legacy` is the duplicate. Remove it so we don't migrate twice.
        let _ = std::fs::remove_file(&legacy);
        return Ok(());
    }
    ensure_dir(&working_dir)?;
    std::fs::rename(&legacy, &working_index).map_err(|source| StoreError::Io {
        path: working_index,
        source,
    })?;
    tracing::info!(
        "migrated .basemind/index.msgpack → .basemind/views/{VIEW_WORKING}/index.msgpack"
    );
    Ok(())
}

fn read_index(view_dir: &Path) -> Result<Option<Index>, StoreError> {
    let path = view_dir.join(INDEX_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).map_err(|source| StoreError::Io {
        path: path.clone(),
        source,
    })?;
    let index: Index = rmp_serde::from_slice(&bytes)?;
    check_schema(index.schema_ver)?;
    Ok(Some(index))
}

fn acquire_lock(basemind_dir: &Path) -> Result<File, StoreError> {
    let path = basemind_dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
    file.try_lock_exclusive()
        .map_err(|_| StoreError::Locked(path))?;
    Ok(file)
}

fn write_blob<T: Serialize>(path: PathBuf, value: &T) -> Result<(), StoreError> {
    // Content-addressed: if the final blob exists, another worker (or a prior scan) already
    // wrote identical bytes. Skip — saves the serialize + write + rename cost, and avoids
    // duplicate-hash races between parallel workers (two distinct source files with the
    // same content hash to the same blob path).
    if path.exists() {
        return Ok(());
    }
    let bytes = rmp_serde::to_vec_named(value)?;
    // Unique tmp suffix per writer thread + process so two workers racing on the same
    // content-hash never share a tmp path. The rename below is atomic on POSIX and
    // will safely clobber any blob that landed in the meantime.
    let suffix = format!(
        "{}.{:?}.tmp",
        std::process::id(),
        std::thread::current().id()
    );
    let tmp = path.with_extension(format!("msgpack.{suffix}"));
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|source| StoreError::Io {
                path: tmp.clone(),
                source,
            })?;
        f.write_all(&bytes).map_err(|source| StoreError::Io {
            path: tmp.clone(),
            source,
        })?;
    }
    if let Err(source) = std::fs::rename(&tmp, &path) {
        // Clean up the orphan tmp so a partially-completed run doesn't leave litter.
        let _ = std::fs::remove_file(&tmp);
        return Err(StoreError::Io {
            path: path.clone(),
            source,
        });
    }
    Ok(())
}

fn check_schema(found: u16) -> Result<(), StoreError> {
    if found == SCHEMA_VER {
        Ok(())
    } else {
        Err(StoreError::SchemaMismatch {
            found,
            expected: SCHEMA_VER,
        })
    }
}
