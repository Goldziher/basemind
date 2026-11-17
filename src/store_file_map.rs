//! The file map on disk (`index.msgpack`): reading it under the version checks, writing it
//! atomically, and the per-view housekeeping around it. Split out of `store.rs` to keep that module
//! under the line cap; every name is re-exported there.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use serde::Deserialize;

use super::{INDEX_FILE, Index, StoreError, VIEW_WORKING, VIEWS_DIR, ensure_dir};
use crate::extract::SCHEMA_VER;

/// Write `index` as the view's file map: a temporary file, fsynced, renamed over the old one, and
/// the rename made durable. A failure leaves the previous file map in place.
pub(super) fn write_file_map(view_dir: &Path, index: &Index) -> Result<(), StoreError> {
    let final_path = view_dir.join(INDEX_FILE);
    let tmp_path = view_dir.join(format!("{INDEX_FILE}.tmp"));
    let bytes = rmp_serde::to_vec_named(index)?;
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
    // The rename is durable only once the directory entry is. Not fatal: the new file map is
    // already in place, and some filesystems refuse to fsync a directory.
    if let Err(error) = crate::store_base::sync_directory(view_dir) {
        tracing::warn!(%error, "could not fsync the view directory after writing the file map");
    }
    Ok(())
}

/// Rename the file map of `view_dir` aside as `index.msgpack.unreadable-<unix seconds>`: its
/// version cannot be read, so it may be a newer basemind's, which is never deleted.
pub(super) fn set_aside_unreadable_map(view_dir: &Path) -> Result<(), StoreError> {
    let from = view_dir.join(INDEX_FILE);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let to = view_dir.join(format!("{INDEX_FILE}.unreadable-{secs}"));
    std::fs::rename(&from, &to).map_err(|source| StoreError::Io { path: from, source })?;
    tracing::warn!(path = %to.display(), "file map unreadable; set aside, and the view is rebuilt");
    Ok(())
}

/// Delete the index file in a single view's directory.
pub(crate) fn wipe_view(view_dir: &Path) -> Result<(), StoreError> {
    let index_path = view_dir.join(INDEX_FILE);
    if index_path.exists() {
        std::fs::remove_file(&index_path).map_err(|source| StoreError::Io {
            path: index_path,
            source,
        })?;
    }
    Ok(())
}

/// Pre-views installs kept `index.msgpack` at the top of `.basemind/`. After the upgrade,
/// each view lives under `.basemind/views/<view>/`. If we detect the legacy file AND no
/// working-view file exists yet, move it in place. Idempotent: re-runs are no-ops.
pub(super) fn migrate_legacy_index_into_views(basemind_dir: &Path) -> Result<(), StoreError> {
    let legacy = basemind_dir.join(INDEX_FILE);
    if !legacy.exists() {
        return Ok(());
    }
    let working_dir = basemind_dir.join(VIEWS_DIR).join(VIEW_WORKING);
    let working_index = working_dir.join(INDEX_FILE);
    if working_index.exists() {
        let _ = std::fs::remove_file(&legacy);
        return Ok(());
    }
    ensure_dir(&working_dir)?;
    std::fs::rename(&legacy, &working_index).map_err(|source| StoreError::Io {
        path: working_index,
        source,
    })?;
    tracing::info!("migrated .basemind/index.msgpack → .basemind/views/{VIEW_WORKING}/index.msgpack");
    Ok(())
}

/// Read and deserialize a view's `index.msgpack`. `Ok(None)` when the file is absent;
/// `Err(StoreError::SchemaMismatch)` on an older version, `Err(StoreError::NewerSchema)` on a newer
/// schema or blob layout, `Err(StoreError::StaleBlobLayout)` on a map of an earlier blob store
/// layout, `Err(StoreError::Decode)` on a map of this version that does not decode, and
/// `Err(StoreError::UnreadableFileMap)` on one whose version cannot be read. Reused by `store_gc` to
/// enumerate the live blob hashes referenced by every view.
pub(crate) fn read_index(view_dir: &Path) -> Result<Option<Index>, StoreError> {
    /// The fields that say which basemind wrote the map: `schema_ver`, which every layout has had,
    /// and `blob_layout`, absent (`0`) before blob namespaces.
    #[derive(Deserialize)]
    struct Version {
        schema_ver: u16,
        #[serde(default)]
        blob_layout: u8,
    }
    let path = view_dir.join(INDEX_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).map_err(|source| StoreError::Io {
        path: path.clone(),
        source,
    })?;
    // The version is read on its own first: a newer map need not decode as this one, and a decode
    // failure must not read as a damaged map that may be removed. Only a map whose version reads as
    // this binary's is rebuildable when the rest does not decode. ~keep
    let Ok(Version {
        schema_ver,
        blob_layout,
    }) = rmp_serde::from_slice(&bytes)
    else {
        return Err(StoreError::UnreadableFileMap { path });
    };
    check_schema(schema_ver)?;
    if blob_layout > crate::store_layout::BLOB_LAYOUT {
        return Err(StoreError::NewerSchema {
            stored: u16::from(blob_layout),
            current: u16::from(crate::store_layout::BLOB_LAYOUT),
        });
    }
    let index: Index = rmp_serde::from_slice(&bytes)?;
    check_schema(index.schema_ver)?;
    if index.blob_layout > crate::store_layout::BLOB_LAYOUT {
        return Err(StoreError::NewerSchema {
            stored: u16::from(index.blob_layout),
            current: u16::from(crate::store_layout::BLOB_LAYOUT),
        });
    }
    if index.blob_layout < crate::store_layout::BLOB_LAYOUT {
        return Err(StoreError::StaleBlobLayout {
            found: index.blob_layout,
            expected: crate::store_layout::BLOB_LAYOUT,
        });
    }
    Ok(Some(index))
}

/// Guard every blob / index read against a foreign on-disk schema: an older one is a rebuildable
/// [`StoreError::SchemaMismatch`], a newer one a [`StoreError::NewerSchema`] no caller may wipe.
/// `pub(crate)` because the blob accessors that call it live in [`crate::store_blob`].
pub(crate) fn check_schema(found: u16) -> Result<(), StoreError> {
    match found.cmp(&SCHEMA_VER) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Less => Err(StoreError::SchemaMismatch {
            found,
            expected: SCHEMA_VER,
        }),
        std::cmp::Ordering::Greater => Err(StoreError::NewerSchema {
            stored: found,
            current: SCHEMA_VER,
        }),
    }
}
