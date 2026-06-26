//! Blob (de)framing + atomic write for the content-addressed extraction store.
//!
//! Each indexed source file persists one combined-filemap blob `<hash>.fm.msgpack`, framed
//! `[l1_len: u32 LE][l1 msgpack][l2 msgpack | empty]` — the L1 outline and (when extracted
//! eagerly) the L2 calls in a single content-addressed file. Fusing the two tiers halves the
//! per-file blob writes (`open` + atomic `rename`) on the default eager-L2 scan; the
//! length-prefix lets the common outline-only read decode just the L1 slice without touching
//! L2. The doc tier (`write_blob`) stays a plain unframed msgpack blob.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[cfg(feature = "documents")]
use crate::extract::SCHEMA_VER;
use crate::extract::{FileMapL1, FileMapL2};
use crate::store::StoreError;

/// Minimal peek struct: decode only a blob's leading `schema_ver` field. Every blob map
/// (`FileMapL1` / `FileMapL2` / `FileMapDoc`) carries `schema_ver: u16` first; rmp-serde
/// decodes named maps by field name and ignores the remaining (unknown-to-us) fields, so
/// this reads the version without paying to decode the whole blob.
#[derive(Deserialize)]
struct BlobSchemaPeek {
    schema_ver: u16,
}

/// Read a file's bytes, mapping a missing file to `Ok(None)`. One `read` syscall instead of
/// the `exists()` + `read` TOCTOU pair the blob readers used before.
pub(crate) fn read_if_exists(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(StoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Split a combined-filemap frame `[l1_len: u32 LE][l1][l2]` into its `(l1, l2)` byte slices.
/// `l2` is empty when the file carries no call tier. Returns `None` when the 4-byte header is
/// missing or claims more L1 bytes than the frame holds (corrupt / truncated blob).
fn frame_slices(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let header: [u8; 4] = bytes.get(0..4)?.try_into().ok()?;
    let l1_len = u32::from_le_bytes(header) as usize;
    let rest = bytes.get(4..)?;
    let l1 = rest.get(..l1_len)?;
    let l2 = &rest[l1_len..];
    Some((l1, l2))
}

/// Serialize both extraction tiers into one frame. `l2 = None` yields an empty L2 slice.
pub(crate) fn frame_filemap(l1: &FileMapL1, l2: Option<&FileMapL2>) -> Result<Vec<u8>, StoreError> {
    let l1_bytes = rmp_serde::to_vec_named(l1)?;
    let l2_bytes = match l2 {
        Some(map) => rmp_serde::to_vec_named(map)?,
        None => Vec::new(),
    };
    let l1_len = u32::try_from(l1_bytes.len()).map_err(|_| StoreError::BlobTooLarge)?;
    let mut out = Vec::with_capacity(4 + l1_bytes.len() + l2_bytes.len());
    out.extend_from_slice(&l1_len.to_le_bytes());
    out.extend_from_slice(&l1_bytes);
    out.extend_from_slice(&l2_bytes);
    Ok(out)
}

/// Decode the L1 outline from a frame, leaving the trailing L2 bytes untouched.
pub(crate) fn parse_filemap_l1(path: &Path, bytes: &[u8]) -> Result<FileMapL1, StoreError> {
    let (l1, _l2) = frame_slices(bytes).ok_or_else(|| StoreError::CorruptBlob {
        path: path.to_path_buf(),
    })?;
    Ok(rmp_serde::from_slice(l1)?)
}

/// Decode the L2 calls from a frame; `Ok(None)` when the file carries no call tier.
pub(crate) fn parse_filemap_l2(path: &Path, bytes: &[u8]) -> Result<Option<FileMapL2>, StoreError> {
    let (_l1, l2) = frame_slices(bytes).ok_or_else(|| StoreError::CorruptBlob {
        path: path.to_path_buf(),
    })?;
    if l2.is_empty() {
        return Ok(None);
    }
    Ok(Some(rmp_serde::from_slice(l2)?))
}

/// Cheaply read a combined-filemap blob's persisted `schema_ver` from the frame's L1 slice.
/// Returns `None` if the blob is unreadable or malformed (treated as "not current", forcing a
/// rewrite).
pub(crate) fn peek_filemap_schema(path: &Path) -> Option<u16> {
    let bytes = std::fs::read(path).ok()?;
    let (l1, _l2) = frame_slices(&bytes)?;
    rmp_serde::from_slice::<BlobSchemaPeek>(l1)
        .ok()
        .map(|peek| peek.schema_ver)
}

/// Doc-tier peek: a plain (unframed) msgpack blob whose leading field is `schema_ver`.
#[cfg(feature = "documents")]
fn peek_blob_schema(path: &Path) -> Option<u16> {
    let bytes = std::fs::read(path).ok()?;
    rmp_serde::from_slice::<BlobSchemaPeek>(&bytes)
        .ok()
        .map(|peek| peek.schema_ver)
}

thread_local! {
    /// Per-thread `"<pid>.<thread-id>.tmp"` suffix for blob tmp files. The process id and
    /// thread id never change for the lifetime of a worker thread, so we build the string
    /// once and reuse it across every blob write on that thread.
    static TMP_SUFFIX: String = format!(
        "{}.{:?}.tmp",
        std::process::id(),
        std::thread::current().id()
    );
}

/// Atomic blob write: stream `bytes` to a per-thread-unique tmp file, then POSIX-rename it
/// over `path`. The rename is atomic and safely clobbers any blob that raced in. Shared by
/// the framed-filemap writer and the doc-tier [`write_blob`].
pub(crate) fn write_bytes_atomic(path: PathBuf, bytes: &[u8]) -> Result<(), StoreError> {
    use std::fs::OpenOptions;
    use std::io::Write;

    // Unique tmp suffix per writer thread + process so two workers racing on the same
    // content-hash never share a tmp path. The process-id + thread-id portion is invariant
    // for a given worker thread, so it is cached per thread; only the final per-call
    // extension is formatted on the hot path.
    let tmp = TMP_SUFFIX.with(|suffix| path.with_extension(format!("msgpack.{suffix}")));
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
        f.write_all(bytes).map_err(|source| StoreError::Io {
            path: tmp.clone(),
            source,
        })?;
    }
    if let Err(source) = std::fs::rename(&tmp, &path) {
        // Clean up the orphan tmp so a partially-completed run doesn't leave litter.
        let _ = std::fs::remove_file(&tmp);
        return Err(StoreError::Io { path, source });
    }
    Ok(())
}

/// Doc-tier blob write: content-addressed skip on matching schema, else serialize + atomic
/// write. The combined-filemap blobs go through `Store::write_filemap_hex` instead.
#[cfg(feature = "documents")]
pub(crate) fn write_blob<T: serde::Serialize>(path: PathBuf, value: &T) -> Result<(), StoreError> {
    // A schema bump leaves a stale-schema blob at the same content-hash path; the durable
    // refresh re-extracts and relies on this write to OVERWRITE it. Only short-circuit when
    // the on-disk schema already matches.
    if path.exists() && peek_blob_schema(&path) == Some(SCHEMA_VER) {
        return Ok(());
    }
    let bytes = rmp_serde::to_vec_named(value)?;
    write_bytes_atomic(path, &bytes)
}
