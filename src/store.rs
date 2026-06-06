use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::extract::{FileMapL1, FileMapL2, SCHEMA_VER};
use crate::hashing::{self, Hash};

pub const INDEX_FILE: &str = "index.msgpack";
pub const BLOBS_DIR: &str = "blobs";
pub const LOCK_FILE: &str = ".lock";

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
    #[error("another gitmind process holds the lock on {0} (likely `gitmind watch` is running)")]
    Locked(PathBuf),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Index {
    pub schema_ver: u16,
    /// Relative path (forward-slash separated) → FileEntry. BTreeMap for deterministic serialization.
    pub files: BTreeMap<String, FileEntry>,
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
    pub gitmind_dir: PathBuf,
    pub index: Index,
    _lock: Option<File>,
}

impl Store {
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        let gitmind_dir = root.join(crate::config::GITMIND_DIR);
        ensure_dir(&gitmind_dir)?;
        ensure_dir(&gitmind_dir.join(BLOBS_DIR))?;
        let lock = acquire_lock(&gitmind_dir)?;
        let index = match read_index(&gitmind_dir) {
            Ok(Some(idx)) => idx,
            Ok(None) => Index::empty(),
            Err(StoreError::SchemaMismatch { found, expected }) => {
                tracing::info!(
                    found,
                    expected,
                    "cache schema bumped; wiping .gitmind/index.msgpack and blobs/"
                );
                wipe_cache(&gitmind_dir)?;
                Index::empty()
            }
            Err(e) => return Err(e),
        };
        Ok(Self {
            root: root.to_path_buf(),
            gitmind_dir,
            index,
            _lock: Some(lock),
        })
    }

    /// Open without taking the exclusive lock. Use for read-only consumers (CLI query, MCP --attach).
    pub fn open_read_only(root: &Path) -> Result<Self, StoreError> {
        let gitmind_dir = root.join(crate::config::GITMIND_DIR);
        let index = read_index(&gitmind_dir)?.unwrap_or_else(Index::empty);
        Ok(Self {
            root: root.to_path_buf(),
            gitmind_dir,
            index,
            _lock: None,
        })
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
        self.gitmind_dir
            .join(BLOBS_DIR)
            .join(format!("{hash_hex}.l1.msgpack"))
    }

    pub fn blob_path_l2_hex(&self, hash_hex: &str) -> PathBuf {
        self.gitmind_dir
            .join(BLOBS_DIR)
            .join(format!("{hash_hex}.l2.msgpack"))
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

    pub fn upsert(&mut self, rel: &str, entry: FileEntry) {
        self.index.files.insert(rel.to_string(), entry);
    }

    pub fn remove(&mut self, rel: &str) {
        self.index.files.remove(rel);
    }

    pub fn lookup(&self, rel: &str) -> Option<&FileEntry> {
        self.index.files.get(rel)
    }

    /// Atomically rewrite the index file (tmp + rename).
    pub fn flush(&self) -> Result<(), StoreError> {
        let final_path = self.gitmind_dir.join(INDEX_FILE);
        let tmp_path = self.gitmind_dir.join(format!("{INDEX_FILE}.tmp"));
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

/// Delete the index file and every blob; leave the lock + config alone.
/// Called from `Store::open` when the persisted schema version doesn't match `SCHEMA_VER`.
fn wipe_cache(gitmind_dir: &Path) -> Result<(), StoreError> {
    let index_path = gitmind_dir.join(INDEX_FILE);
    if index_path.exists() {
        std::fs::remove_file(&index_path).map_err(|source| StoreError::Io {
            path: index_path,
            source,
        })?;
    }
    let blobs_dir = gitmind_dir.join(BLOBS_DIR);
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

fn read_index(gitmind_dir: &Path) -> Result<Option<Index>, StoreError> {
    let path = gitmind_dir.join(INDEX_FILE);
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

fn acquire_lock(gitmind_dir: &Path) -> Result<File, StoreError> {
    let path = gitmind_dir.join(LOCK_FILE);
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
