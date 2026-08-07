//! Blob (de)framing + atomic write for the content-addressed extraction store, and the
//! [`Store`] accessors layered over them.
//!
//! New blobs carry a plain `BMB1 | codec | kind | schema` prefix followed by zstd-1 payloads.
//! Combined filemaps compress L1 and L2 as separate frames, preserving the outline-only read
//! path's ability to decode L1 without decompressing L2. Single-map doc / resolution / chunk
//! blobs use one compressed frame; chunk embedding metadata stays in the plain header so the
//! unchanged-file fast path never decompresses chunk text. Readers still accept legacy raw
//! msgpack and `[l1_len][l1][l2]` filemap blobs left by older releases.
//!
//! The per-tier `Store::{blob_path,read,write}_*` methods moved here from `store.rs` (which was
//! over the 1000-line module cap): they are the blob store's read/write surface — one tier per
//! blob suffix (`.fm` / `.doc` / `.rref` / `.chunk`) — and change for the same reason the framing
//! does. They stay inherent methods on [`Store`], so every call site is unaffected by the move.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::extract::SCHEMA_VER;
use crate::extract::{FileMapL1, FileMapL2};
use crate::hashing::{self, Hash};
use crate::store::{Store, StoreError, check_schema};
#[cfg(feature = "code-search")]
use crate::store_blob_codec::read_u16;
use crate::store_blob_codec::{
    BlobKind, CompressedPayload, checked_u32_len, compress_payload, corrupt_blob, decompress_payload, encode_prefix,
    envelope_prefix, read_u32,
};

const SINGLE_HEADER_LEN: usize = 16;
const FILEMAP_HEADER_LEN: usize = 24;
#[cfg(feature = "code-search")]
const CHUNK_HEADER_LEN: usize = 28;
#[cfg(feature = "code-search")]
const MAX_PEEK_ITEMS: usize = 10_000_000;

struct FileMapEnvelope<'a> {
    l1: CompressedPayload<'a>,
    l2: CompressedPayload<'a>,
}

#[cfg(feature = "code-search")]
struct ChunkEnvelopePeek {
    schema_ver: u16,
    embedding_dim: u16,
    embedding_model: String,
    chunk_count: usize,
    embedding_count: usize,
}

#[cfg(feature = "code-search")]
struct ChunkEnvelope<'a> {
    peek: ChunkEnvelopePeek,
    payload: CompressedPayload<'a>,
}

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

/// Split a legacy combined-filemap frame `[l1_len: u32 LE][l1][l2]` into byte slices.
fn frame_slices(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let header: [u8; 4] = bytes.get(0..4)?.try_into().ok()?;
    let l1_len = u32::from_le_bytes(header) as usize;
    let rest = bytes.get(4..)?;
    let l1 = rest.get(..l1_len)?;
    let l2 = &rest[l1_len..];
    Some((l1, l2))
}

fn filemap_envelope<'a>(path: &Path, bytes: &'a [u8]) -> Result<FileMapEnvelope<'a>, StoreError> {
    let prefix = envelope_prefix(path, bytes)?.ok_or_else(|| corrupt_blob(path))?;
    if prefix.kind != BlobKind::FileMap {
        return Err(corrupt_blob(path));
    }
    let l1_raw_len = read_u32(path, bytes, 8)?;
    let l1_stored_len = read_u32(path, bytes, 12)?;
    let l2_raw_len = read_u32(path, bytes, 16)?;
    let l2_stored_len = read_u32(path, bytes, 20)?;
    let l1_end = FILEMAP_HEADER_LEN
        .checked_add(l1_stored_len)
        .ok_or_else(|| corrupt_blob(path))?;
    let l2_end = l1_end.checked_add(l2_stored_len).ok_or_else(|| corrupt_blob(path))?;
    if l2_end != bytes.len() {
        return Err(corrupt_blob(path));
    }
    Ok(FileMapEnvelope {
        l1: CompressedPayload {
            uncompressed_len: l1_raw_len,
            bytes: &bytes[FILEMAP_HEADER_LEN..l1_end],
        },
        l2: CompressedPayload {
            uncompressed_len: l2_raw_len,
            bytes: &bytes[l1_end..l2_end],
        },
    })
}

/// Serialize both extraction tiers into separately-compressed frames. `l2 = None` yields an
/// empty L2 frame so L1-only reads never decompress trailing call data.
pub(crate) fn frame_filemap(l1: &FileMapL1, l2: Option<&FileMapL2>) -> Result<Vec<u8>, StoreError> {
    let l1_bytes = rmp_serde::to_vec_named(l1)?;
    let l2_bytes = match l2 {
        Some(map) => rmp_serde::to_vec_named(map)?,
        None => Vec::new(),
    };
    let l1_compressed = compress_payload(&l1_bytes)?;
    let l2_compressed = if l2_bytes.is_empty() {
        Vec::new()
    } else {
        compress_payload(&l2_bytes)?
    };
    let mut out = Vec::with_capacity(FILEMAP_HEADER_LEN + l1_compressed.len() + l2_compressed.len());
    encode_prefix(BlobKind::FileMap, l1.schema_ver, &mut out);
    out.extend_from_slice(&checked_u32_len(l1_bytes.len())?.to_le_bytes());
    out.extend_from_slice(&checked_u32_len(l1_compressed.len())?.to_le_bytes());
    out.extend_from_slice(&checked_u32_len(l2_bytes.len())?.to_le_bytes());
    out.extend_from_slice(&checked_u32_len(l2_compressed.len())?.to_le_bytes());
    out.extend_from_slice(&l1_compressed);
    out.extend_from_slice(&l2_compressed);
    Ok(out)
}

/// Decode the L1 outline, leaving the separately-compressed L2 frame untouched.
pub(crate) fn parse_filemap_l1(path: &Path, bytes: &[u8]) -> Result<FileMapL1, StoreError> {
    let l1 = match envelope_prefix(path, bytes)? {
        Some(_) => Cow::Owned(decompress_payload(path, filemap_envelope(path, bytes)?.l1)?),
        None => Cow::Borrowed(frame_slices(bytes).ok_or_else(|| corrupt_blob(path))?.0),
    };
    Ok(rmp_serde::from_slice(&l1)?)
}

/// Decode the L2 calls; `Ok(None)` when the file carries no call tier.
pub(crate) fn parse_filemap_l2(path: &Path, bytes: &[u8]) -> Result<Option<FileMapL2>, StoreError> {
    let l2 = match envelope_prefix(path, bytes)? {
        Some(_) => {
            let envelope = filemap_envelope(path, bytes)?;
            if envelope.l2.uncompressed_len == 0 && envelope.l2.bytes.is_empty() {
                return Ok(None);
            }
            Cow::Owned(decompress_payload(path, envelope.l2)?)
        }
        None => Cow::Borrowed(frame_slices(bytes).ok_or_else(|| corrupt_blob(path))?.1),
    };
    if l2.is_empty() {
        Ok(None)
    } else {
        Ok(Some(rmp_serde::from_slice(&l2)?))
    }
}

fn encode_single_blob(bytes: &[u8], schema_ver: u16) -> Result<Vec<u8>, StoreError> {
    let compressed = compress_payload(bytes)?;
    let mut out = Vec::with_capacity(SINGLE_HEADER_LEN + compressed.len());
    encode_prefix(BlobKind::Single, schema_ver, &mut out);
    out.extend_from_slice(&checked_u32_len(bytes.len())?.to_le_bytes());
    out.extend_from_slice(&checked_u32_len(compressed.len())?.to_le_bytes());
    out.extend_from_slice(&compressed);
    Ok(out)
}

fn single_payload<'a>(path: &Path, bytes: &'a [u8], expected: BlobKind) -> Result<CompressedPayload<'a>, StoreError> {
    let prefix = envelope_prefix(path, bytes)?.ok_or_else(|| corrupt_blob(path))?;
    if prefix.kind != expected {
        return Err(corrupt_blob(path));
    }
    let uncompressed_len = read_u32(path, bytes, 8)?;
    let stored_len = read_u32(path, bytes, 12)?;
    let payload_end = SINGLE_HEADER_LEN
        .checked_add(stored_len)
        .ok_or_else(|| corrupt_blob(path))?;
    if payload_end != bytes.len() {
        return Err(corrupt_blob(path));
    }
    Ok(CompressedPayload {
        uncompressed_len,
        bytes: &bytes[SINGLE_HEADER_LEN..payload_end],
    })
}

fn decode_single_or_legacy<'a>(path: &Path, bytes: &'a [u8], expected: BlobKind) -> Result<Cow<'a, [u8]>, StoreError> {
    match envelope_prefix(path, bytes)? {
        Some(_) => Ok(Cow::Owned(decompress_payload(
            path,
            single_payload(path, bytes, expected)?,
        )?)),
        None => Ok(Cow::Borrowed(bytes)),
    }
}

fn is_current_single_blob(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Ok(Some(prefix)) = envelope_prefix(path, &bytes) else {
        return false;
    };
    if prefix.kind != BlobKind::Single || prefix.schema_ver != SCHEMA_VER {
        return false;
    }
    let Ok(decoded) = decode_single_or_legacy(path, &bytes, BlobKind::Single) else {
        return false;
    };
    rmp_serde::from_slice::<BlobSchemaPeek>(&decoded).is_ok_and(|peek| peek.schema_ver == SCHEMA_VER)
}

fn is_current_filemap_blob(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Ok(Some(prefix)) = envelope_prefix(path, &bytes) else {
        return false;
    };
    if prefix.kind != BlobKind::FileMap || prefix.schema_ver != SCHEMA_VER {
        return false;
    }
    let Ok(l1) = parse_filemap_l1(path, &bytes) else {
        return false;
    };
    if l1.schema_ver != SCHEMA_VER {
        return false;
    }
    match parse_filemap_l2(path, &bytes) {
        Ok(Some(l2)) => l2.schema_ver == SCHEMA_VER,
        Ok(None) => true,
        Err(_) => false,
    }
}

#[cfg(feature = "code-search")]
fn encode_chunk_blob(blob: &crate::chunk::CodeChunkBlob) -> Result<Vec<u8>, StoreError> {
    let bytes = rmp_serde::to_vec_named(blob)?;
    let compressed = compress_payload(&bytes)?;
    let model = blob.embedding_model.as_bytes();
    let model_len = u16::try_from(model.len()).map_err(|_| StoreError::BlobTooLarge)?;
    let mut out = Vec::with_capacity(CHUNK_HEADER_LEN + model.len() + compressed.len());
    encode_prefix(BlobKind::Chunk, blob.schema_ver, &mut out);
    out.extend_from_slice(&checked_u32_len(bytes.len())?.to_le_bytes());
    out.extend_from_slice(&checked_u32_len(compressed.len())?.to_le_bytes());
    out.extend_from_slice(&blob.embedding_dim.to_le_bytes());
    out.extend_from_slice(&model_len.to_le_bytes());
    out.extend_from_slice(&checked_u32_len(blob.chunks.len())?.to_le_bytes());
    out.extend_from_slice(&checked_u32_len(blob.embeddings.len())?.to_le_bytes());
    out.extend_from_slice(model);
    out.extend_from_slice(&compressed);
    Ok(out)
}

#[cfg(feature = "code-search")]
fn chunk_envelope<'a>(path: &Path, bytes: &'a [u8]) -> Result<ChunkEnvelope<'a>, StoreError> {
    let prefix = envelope_prefix(path, bytes)?.ok_or_else(|| corrupt_blob(path))?;
    if prefix.kind != BlobKind::Chunk {
        return Err(corrupt_blob(path));
    }
    let uncompressed_len = read_u32(path, bytes, 8)?;
    let stored_len = read_u32(path, bytes, 12)?;
    let embedding_dim = read_u16(path, bytes, 16)?;
    let model_len = read_u16(path, bytes, 18)? as usize;
    let chunk_count = read_u32(path, bytes, 20)?;
    let embedding_count = read_u32(path, bytes, 24)?;
    if chunk_count > MAX_PEEK_ITEMS || embedding_count > MAX_PEEK_ITEMS {
        return Err(corrupt_blob(path));
    }
    let model_end = CHUNK_HEADER_LEN
        .checked_add(model_len)
        .ok_or_else(|| corrupt_blob(path))?;
    let payload_end = model_end.checked_add(stored_len).ok_or_else(|| corrupt_blob(path))?;
    if payload_end != bytes.len() {
        return Err(corrupt_blob(path));
    }
    let embedding_model = std::str::from_utf8(&bytes[CHUNK_HEADER_LEN..model_end])
        .map_err(|_| corrupt_blob(path))?
        .to_string();
    Ok(ChunkEnvelope {
        peek: ChunkEnvelopePeek {
            schema_ver: prefix.schema_ver,
            embedding_dim,
            embedding_model,
            chunk_count,
            embedding_count,
        },
        payload: CompressedPayload {
            uncompressed_len,
            bytes: &bytes[model_end..payload_end],
        },
    })
}

#[cfg(feature = "code-search")]
fn decode_chunk_or_legacy<'a>(path: &Path, bytes: &'a [u8]) -> Result<Cow<'a, [u8]>, StoreError> {
    match envelope_prefix(path, bytes)? {
        Some(_) => Ok(Cow::Owned(decompress_payload(
            path,
            chunk_envelope(path, bytes)?.payload,
        )?)),
        None => Ok(Cow::Borrowed(bytes)),
    }
}

#[cfg(feature = "code-search")]
fn public_chunk_peek(header: ChunkEnvelopePeek) -> crate::chunk::CodeChunkBlobPeek {
    crate::chunk::CodeChunkBlobPeek {
        schema_ver: header.schema_ver,
        embedding_dim: header.embedding_dim,
        embedding_model: header.embedding_model,
        chunks: (0..header.chunk_count).map(|_| serde::de::IgnoredAny).collect(),
        embeddings: (0..header.embedding_count).map(|_| serde::de::IgnoredAny).collect(),
    }
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
        let _ = std::fs::remove_file(&tmp);
        return Err(StoreError::Io { path, source });
    }
    Ok(())
}

/// Compressed single-map blob write (doc tier + resolution tier): content-addressed skip on a
/// current envelope, else serialize + atomic write. The combined-filemap blobs go through
/// `Store::write_filemap_hex` instead; a legacy raw blob is rewritten on its next write.
pub(crate) fn write_blob<T: serde::Serialize>(path: PathBuf, value: &T) -> Result<(), StoreError> {
    if is_current_single_blob(&path) {
        return Ok(());
    }
    let bytes = rmp_serde::to_vec_named(value)?;
    let encoded = encode_single_blob(&bytes, SCHEMA_VER)?;
    write_bytes_atomic(path, &encoded)
}

/// Like [`write_blob`] but always (re)writes, even when a same-schema document blob already exists.
/// A vectorless `Deferred` blob (`embedding_dim: 0`) can later be upgraded to an embedded `Inline`
/// blob for the same content hash, so a schema-only skip would preserve stale embedding state.
#[cfg(feature = "documents")]
pub(crate) fn write_blob_overwrite<T: serde::Serialize>(path: PathBuf, value: &T) -> Result<(), StoreError> {
    let bytes = rmp_serde::to_vec_named(value)?;
    let encoded = encode_single_blob(&bytes, SCHEMA_VER)?;
    write_bytes_atomic(path, &encoded)
}

/// The blob store's read/write surface: one accessor group per content-addressed tier, all keyed
/// by the source file's content hash under [`Store::blobs_dir`].
impl Store {
    pub fn blob_path_fm(&self, hash: &Hash) -> PathBuf {
        let buf = hashing::hex_buf(hash);
        self.blob_path_fm_hex(hashing::hex_str(&buf))
    }

    /// Build the combined-filemap blob path from an already-hex-encoded hash. One blob per
    /// source file holds both the L1 outline and (when extracted) the L2 calls, framed as
    /// `[l1_len: u32 LE][l1 msgpack][l2 msgpack | empty]`. Skips the encode round-trip when
    /// the caller starts from a `FileEntry::hash_hex`.
    pub fn blob_path_fm_hex(&self, hash_hex: &str) -> PathBuf {
        self.blobs_dir.join(format!("{hash_hex}.fm.msgpack"))
    }

    #[cfg(feature = "documents")]
    pub fn blob_path_doc(&self, hash: &Hash) -> PathBuf {
        let buf = hashing::hex_buf(hash);
        self.blob_path_doc_hex(hashing::hex_str(&buf))
    }

    #[cfg(feature = "documents")]
    pub fn blob_path_doc_hex(&self, hash_hex: &str) -> PathBuf {
        self.blobs_dir.join(format!("{hash_hex}.doc.msgpack"))
    }

    /// Read the L1 outline from the combined-filemap blob. Deserializes only the L1 slice of
    /// the frame — the trailing L2 bytes are read off disk but never decoded, so the common
    /// outline-only read path (`MapCache` build, `search_symbols`) pays no L2 decode cost.
    pub fn read_l1_by_hex(&self, hash_hex: &str) -> Result<Option<FileMapL1>, StoreError> {
        let path = self.blob_path_fm_hex(hash_hex);
        let Some(bytes) = read_if_exists(&path)? else {
            return Ok(None);
        };
        let map = parse_filemap_l1(&path, &bytes)?;
        check_schema(map.schema_ver)?;
        Ok(Some(map))
    }

    /// Read the L2 calls from the combined-filemap blob. Returns `Ok(None)` both when the blob
    /// is absent and when it carries no L2 tier (the file was scanned with `eager_l2 = false`
    /// or L2 extraction failed) — callers escalate via `query::file_outline_l2`.
    pub fn read_l2_by_hex(&self, hash_hex: &str) -> Result<Option<FileMapL2>, StoreError> {
        let path = self.blob_path_fm_hex(hash_hex);
        let Some(bytes) = read_if_exists(&path)? else {
            return Ok(None);
        };
        match parse_filemap_l2(&path, &bytes)? {
            Some(map) => {
                check_schema(map.schema_ver)?;
                Ok(Some(map))
            }
            None => Ok(None),
        }
    }

    /// Write the combined-filemap blob for a file. Holds both tiers in one content-addressed blob,
    /// with independently compressed L1 and L2 frames, so the default eager-L2 scan does one
    /// `open` + `write` + atomic `rename` per file without making L1-only reads decompress L2.
    /// `l2 = None` writes an empty L2 frame.
    pub fn write_filemap_hex(&self, hash_hex: &str, l1: &FileMapL1, l2: Option<&FileMapL2>) -> Result<(), StoreError> {
        let path = self.blob_path_fm_hex(hash_hex);
        if is_current_filemap_blob(&path) {
            return Ok(());
        }
        let bytes = frame_filemap(l1, l2)?;
        write_bytes_atomic(path, &bytes)
    }

    /// Write a document blob. Always overwrites (issue #44): this call is only reached after
    /// `cached_doc_is_reusable` rejected the existing blob — e.g. a vectorless `Deferred` blob being
    /// upgraded by an embedded `Inline` re-extraction of the SAME content hash — so the old
    /// schema-only skip could only ever preserve a blob the caller had just decided was inadequate,
    /// leaving it vectorless forever and re-embedding on every entry-less encounter. A `Deferred`
    /// pass cannot downgrade an embedded blob this way, because `cached_doc_is_reusable` accepts any
    /// readable blob when embedding is off (the reuse branch returns before this write).
    #[cfg(feature = "documents")]
    pub fn write_doc(&self, hash: &Hash, map: &crate::extract::doc::FileMapDoc) -> Result<(), StoreError> {
        write_blob_overwrite(self.blob_path_doc(hash), map)
    }

    #[cfg(feature = "documents")]
    pub fn read_doc_by_hex(&self, hash_hex: &str) -> Result<Option<crate::extract::doc::FileMapDoc>, StoreError> {
        let path = self.blob_path_doc_hex(hash_hex);
        let Some(bytes) = read_if_exists(&path)? else {
            return Ok(None);
        };
        let decoded = decode_single_or_legacy(&path, &bytes, BlobKind::Single)?;
        let map: crate::extract::doc::FileMapDoc = rmp_serde::from_slice(&decoded)?;
        check_schema(map.schema_ver)?;
        Ok(Some(map))
    }

    /// Refresh a REUSED document blob's mtime to now (best-effort).
    ///
    /// The blob GC keeps an *unreferenced* blob only while its mtime is within `BLOB_GC_GRACE`
    /// (`src/store_gc.rs`). A long-lived document whose content never changes is reused, never
    /// rewritten, so its blob mtime stays frozen at first-write and eventually ages past the grace —
    /// and then a `NoCache` rename's transient entry-less window (issue #44: the remove half drops the
    /// `DocEntry` before the create half re-references the same hash) lets the sweep reap the blob,
    /// forcing a full re-extract + re-embed on the next encounter. Bumping mtime on every reuse keeps
    /// an actively-scanned doc's blob "young", so the grace actually covers those rename windows.
    /// Best-effort: a failure (blob already gone, read-only fs) only forgoes the protection for this
    /// cycle — it never fails the scan.
    #[cfg(feature = "documents")]
    pub fn touch_doc_blob(&self, hash_hex: &str) {
        let path = self.blob_path_doc_hex(hash_hex);
        if let Err(error) = std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(std::time::SystemTime::now()))
        {
            tracing::debug!(%error, path = %path.display(), "touch_doc_blob: refreshing mtime failed");
        }
    }

    /// Path of a file's resolution blob (`<hash>.rref.msgpack`) — the per-file code-intelligence
    /// facts (intra-file resolved edges + import/export list). A sibling of the `.fm`/`.doc`
    /// blobs, content-addressed by source hash and stored as compressed single-map msgpack.
    pub fn blob_path_rref_hex(&self, hash_hex: &str) -> PathBuf {
        self.blobs_dir.join(format!("{hash_hex}.rref.msgpack"))
    }

    /// Write a file's resolution facts. Content-addressed skip on matching schema (identical
    /// source bytes already analyzed), else serialize + atomic write — mirrors `write_doc`.
    pub fn write_resolved_hex(
        &self,
        hash_hex: &str,
        refs: &crate::intel::model::FileResolvedRefs,
    ) -> Result<(), StoreError> {
        write_blob(self.blob_path_rref_hex(hash_hex), refs)
    }

    /// Read a file's resolution facts. `Ok(None)` when the file has no resolution blob (never
    /// analyzed, or produced no facts). A schema mismatch surfaces as an error so the second pass
    /// recomputes rather than trusting a stale blob.
    pub fn read_resolved_by_hex(
        &self,
        hash_hex: &str,
    ) -> Result<Option<crate::intel::model::FileResolvedRefs>, StoreError> {
        let path = self.blob_path_rref_hex(hash_hex);
        let Some(bytes) = read_if_exists(&path)? else {
            return Ok(None);
        };
        let decoded = decode_single_or_legacy(&path, &bytes, BlobKind::Single)?;
        let refs: crate::intel::model::FileResolvedRefs = rmp_serde::from_slice(&decoded)?;
        check_schema(refs.schema_ver)?;
        Ok(Some(refs))
    }

    /// Path of a file's code-chunk sidecar (`<hash>.chunk.msgpack`) — the per-file chunk list +
    /// embeddings that back the semantic code-search tier. A sibling of the `.fm`/`.doc`/`.rref`
    /// blobs, content-addressed by source hash. The msgpack payload is compressed; its embedding
    /// state remains in the plain envelope header for the unchanged-file fast path.
    #[cfg(feature = "code-search")]
    pub fn blob_path_chunk_hex(&self, hash_hex: &str) -> PathBuf {
        self.blobs_dir.join(format!("{hash_hex}.chunk.msgpack"))
    }

    /// Write a file's code-chunk sidecar. Always overwrites: unlike the other content-addressed
    /// blobs, a chunk sidecar's embedding payload varies for the SAME content hash — a `Deferred`
    /// pass writes it chunk-only (`embedding_dim: 0`) and a later `Inline` pass upgrades it in place.
    /// A schema-only skip would keep the unembedded blob. Re-embedding of a genuinely-unchanged file
    /// is prevented upstream by `embed_state_satisfied`, not here.
    #[cfg(feature = "code-search")]
    pub fn write_chunks_hex(&self, hash_hex: &str, blob: &crate::chunk::CodeChunkBlob) -> Result<(), StoreError> {
        let encoded = encode_chunk_blob(blob)?;
        write_bytes_atomic(self.blob_path_chunk_hex(hash_hex), &encoded)
    }

    /// Read a file's code-chunk sidecar. `Ok(None)` when the file has no chunk blob (never
    /// chunked, or produced no chunks). A schema mismatch surfaces as an error so the scanner
    /// re-chunks rather than trusting a stale blob.
    #[cfg(feature = "code-search")]
    pub fn read_chunks_by_hex(&self, hash_hex: &str) -> Result<Option<crate::chunk::CodeChunkBlob>, StoreError> {
        let path = self.blob_path_chunk_hex(hash_hex);
        let Some(bytes) = read_if_exists(&path)? else {
            return Ok(None);
        };
        let decoded = decode_chunk_or_legacy(&path, &bytes)?;
        let blob: crate::chunk::CodeChunkBlob = rmp_serde::from_slice(&decoded)?;
        check_schema(blob.schema_ver)?;
        Ok(Some(blob))
    }

    /// Cheaply read a chunk sidecar's embedding state without decoding the chunk text. Same contract
    /// as [`read_chunks_by_hex`](Self::read_chunks_by_hex) — `Ok(None)` when the file has no chunk
    /// blob, a schema mismatch surfaces as an error — but reads only the plain envelope's counts +
    /// embedding dim/model, without decompressing the chunk/embedding payload. Legacy raw blobs use
    /// the prior partial-msgpack decode. Backs the `embed_state_satisfied` unchanged-file fast path.
    #[cfg(feature = "code-search")]
    pub fn peek_chunk_state(&self, hash_hex: &str) -> Result<Option<crate::chunk::CodeChunkBlobPeek>, StoreError> {
        let path = self.blob_path_chunk_hex(hash_hex);
        let Some(bytes) = read_if_exists(&path)? else {
            return Ok(None);
        };
        let peek = match envelope_prefix(&path, &bytes)? {
            Some(_) => public_chunk_peek(chunk_envelope(&path, &bytes)?.peek),
            None => rmp_serde::from_slice(&bytes)?,
        };
        check_schema(peek.schema_ver)?;
        Ok(Some(peek))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{VIEW_WORKING, init_isolated_cache};

    fn sample_l1() -> FileMapL1 {
        FileMapL1 {
            schema_ver: SCHEMA_VER,
            language: "rust".to_string(),
            size_bytes: 42,
            had_errors: false,
            error_count: 0,
            symbols: Vec::new(),
            imports: Vec::new(),
            implementations: Vec::new(),
            rationale: Vec::new(),
        }
    }

    fn sample_l2() -> FileMapL2 {
        FileMapL2 {
            schema_ver: SCHEMA_VER,
            language: "rust".to_string(),
            calls: Vec::new(),
            docs: Vec::new(),
        }
    }

    #[test]
    fn filemap_frame_round_trips_both_tiers() {
        init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash_hex = "a".repeat(64);

        store
            .write_filemap_hex(&hash_hex, &sample_l1(), Some(&sample_l2()))
            .expect("write combined frame");

        let l1 = store.read_l1_by_hex(&hash_hex).expect("read l1");
        assert_eq!(l1.map(|m| m.size_bytes), Some(42), "L1 slice round-trips");
        let l2 = store.read_l2_by_hex(&hash_hex).expect("read l2");
        assert_eq!(l2.map(|m| m.language), Some("rust".to_string()), "L2 present");
    }

    #[test]
    fn filemap_frame_l1_only_reads_back_no_l2() {
        init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash_hex = "b".repeat(64);

        store
            .write_filemap_hex(&hash_hex, &sample_l1(), None)
            .expect("write L1-only frame");

        assert!(
            store.read_l1_by_hex(&hash_hex).expect("read l1").is_some(),
            "L1 present in an L1-only frame"
        );
        assert!(
            store.read_l2_by_hex(&hash_hex).expect("read l2").is_none(),
            "L2 absent in an L1-only frame (escalation will extract on demand)"
        );
    }

    #[test]
    fn new_filemap_blobs_use_a_compressed_self_describing_envelope() {
        init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash_hex = "c".repeat(64);
        let mut l1 = sample_l1();
        l1.language = "rust".repeat(1_024);

        store
            .write_filemap_hex(&hash_hex, &l1, Some(&sample_l2()))
            .expect("write compressed filemap");

        let path = store.blob_path_fm_hex(&hash_hex);
        let persisted = std::fs::read(&path).expect("read persisted filemap");
        let legacy = {
            let l1_bytes = rmp_serde::to_vec_named(&l1).expect("serialize legacy L1");
            let l2_bytes = rmp_serde::to_vec_named(&sample_l2()).expect("serialize legacy L2");
            let mut bytes = Vec::with_capacity(4 + l1_bytes.len() + l2_bytes.len());
            bytes.extend_from_slice(&(l1_bytes.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&l1_bytes);
            bytes.extend_from_slice(&l2_bytes);
            bytes
        };

        assert_eq!(&persisted[..4], b"BMB1", "new blobs carry the envelope magic");
        assert!(
            persisted.len() < legacy.len(),
            "repetitive filemap payload should be smaller after compression"
        );
        assert_eq!(store.read_l1_by_hex(&hash_hex).unwrap(), Some(l1));
        assert_eq!(store.read_l2_by_hex(&hash_hex).unwrap(), Some(sample_l2()));
    }

    #[test]
    fn legacy_uncompressed_filemap_blobs_remain_readable() {
        init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash_hex = "f".repeat(64);
        let l1 = sample_l1();
        let l2 = sample_l2();
        let l1_bytes = rmp_serde::to_vec_named(&l1).expect("serialize legacy L1");
        let l2_bytes = rmp_serde::to_vec_named(&l2).expect("serialize legacy L2");
        let mut legacy = Vec::with_capacity(4 + l1_bytes.len() + l2_bytes.len());
        legacy.extend_from_slice(&(l1_bytes.len() as u32).to_le_bytes());
        legacy.extend_from_slice(&l1_bytes);
        legacy.extend_from_slice(&l2_bytes);
        std::fs::write(store.blob_path_fm_hex(&hash_hex), legacy).expect("write legacy frame");

        assert_eq!(store.read_l1_by_hex(&hash_hex).unwrap(), Some(l1));
        assert_eq!(store.read_l2_by_hex(&hash_hex).unwrap(), Some(l2));
    }

    #[test]
    fn filemap_l1_read_does_not_decompress_a_corrupt_l2_frame() {
        init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash_hex = "2".repeat(64);
        let l1 = sample_l1();
        store
            .write_filemap_hex(&hash_hex, &l1, Some(&sample_l2()))
            .expect("write compressed filemap");

        let path = store.blob_path_fm_hex(&hash_hex);
        let mut persisted = std::fs::read(&path).expect("read filemap bytes");
        let last = persisted.last_mut().expect("L2 compressed frame present");
        *last ^= 0xff;
        std::fs::write(&path, persisted).expect("corrupt only L2 frame");

        assert_eq!(store.read_l1_by_hex(&hash_hex).unwrap(), Some(l1));
        assert!(
            store.read_l2_by_hex(&hash_hex).is_err(),
            "corrupt L2 must fail when requested"
        );
    }

    #[test]
    fn writing_a_current_filemap_repairs_a_corrupt_compressed_payload() {
        init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash_hex = "5".repeat(64);
        let l1 = sample_l1();
        let l2 = sample_l2();
        store
            .write_filemap_hex(&hash_hex, &l1, Some(&l2))
            .expect("write filemap");

        let path = store.blob_path_fm_hex(&hash_hex);
        let mut persisted = std::fs::read(&path).expect("read filemap bytes");
        persisted[FILEMAP_HEADER_LEN] ^= 0xff;
        std::fs::write(&path, persisted).expect("corrupt compressed L1");

        store
            .write_filemap_hex(&hash_hex, &l1, Some(&l2))
            .expect("repair corrupt filemap");
        assert_eq!(store.read_l1_by_hex(&hash_hex).unwrap(), Some(l1));
        assert_eq!(store.read_l2_by_hex(&hash_hex).unwrap(), Some(l2));
    }

    /// Issue #44: a Deferred pass persists the doc blob vectorless (`embedding_dim: 0`); the later
    /// Inline pass re-extracts + embeds and writes the SAME content hash again. That second write
    /// must replace the blob — a schema-only skip keeps it vectorless forever, and every future
    /// entry-less encounter of the content re-embeds again (the re-embed loop).
    #[cfg(feature = "documents")]
    #[test]
    fn write_doc_overwrites_vectorless_blob_with_embedded_doc() {
        use crate::extract::doc::FileMapDoc;
        init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash = crate::hashing::hash_bytes(b"bug-44 deferred-then-inline doc");

        let vectorless = FileMapDoc {
            schema_ver: SCHEMA_VER,
            mime_type: "text/plain".to_string(),
            content: "hello".to_string(),
            metadata: Vec::new(),
            detected_languages: Vec::new(),
            chunks: Vec::new(),
            embedding_model: String::new(),
            embedding_dim: 0,
            keywords: Vec::new(),
            entities: Vec::new(),
            summary: None,
        };
        store.write_doc(&hash, &vectorless).expect("write vectorless blob");

        let embedded = FileMapDoc {
            embedding_model: "balanced".to_string(),
            embedding_dim: 768,
            ..vectorless
        };
        store.write_doc(&hash, &embedded).expect("write embedded blob");

        let hex_buf = hashing::hex_buf(&hash);
        let path = store.blob_path_doc_hex(hashing::hex_str(&hex_buf));
        let persisted = std::fs::read(path).expect("read doc blob bytes");
        assert_eq!(&persisted[..4], b"BMB1", "new document blobs carry the envelope magic");
        let read = store
            .read_doc_by_hex(hashing::hex_str(&hex_buf))
            .expect("read doc blob")
            .expect("doc blob present");
        assert_eq!(
            read.embedding_dim, 768,
            "Inline pass's embedded doc must replace the Deferred pass's vectorless blob (issue #44)"
        );
    }

    #[test]
    fn resolved_blob_round_trips_and_missing_reads_none() {
        use crate::intel::model::{ExportEdge, FileResolvedRefs, ImportEdge, ResolvedEdge};
        init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash_hex = "d".repeat(64);

        let mut refs = FileResolvedRefs::new("typescript");
        refs.intra.push(ResolvedEdge {
            use_start: 40,
            use_end: 43,
            def_start: 4,
            def_end: 7,
        });
        refs.imports.push(ImportEdge {
            local: "foo".to_string(),
            specifier: "./bar".to_string(),
            imported: Some("baz".to_string()),
            is_type: false,
            local_start: 9,
        });
        refs.exports.push(ExportEdge {
            name: "alpha".to_string(),
            name_start: 20,
        });

        store.write_resolved_hex(&hash_hex, &refs).expect("write resolved blob");
        let persisted = std::fs::read(store.blob_path_rref_hex(&hash_hex)).expect("read resolved blob bytes");
        assert_eq!(
            &persisted[..4],
            b"BMB1",
            "new single-map blobs carry the envelope magic"
        );
        let read = store.read_resolved_by_hex(&hash_hex).expect("read resolved blob");
        assert_eq!(read.as_ref(), Some(&refs), "resolution blob round-trips exactly");

        let missing = store.read_resolved_by_hex(&"e".repeat(64)).expect("read missing");
        assert_eq!(missing, None, "absent resolution blob reads back as None");
    }

    #[test]
    fn legacy_uncompressed_single_map_blobs_remain_readable() {
        use crate::intel::model::FileResolvedRefs;

        init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash_hex = "1".repeat(64);
        let refs = FileResolvedRefs::new("rust");
        let legacy = rmp_serde::to_vec_named(&refs).expect("serialize legacy blob");
        std::fs::write(store.blob_path_rref_hex(&hash_hex), legacy).expect("write legacy blob");

        assert_eq!(store.read_resolved_by_hex(&hash_hex).unwrap(), Some(refs));
    }

    #[test]
    fn writing_a_current_single_map_repairs_a_corrupt_compressed_payload() {
        use crate::intel::model::FileResolvedRefs;

        init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash_hex = "6".repeat(64);
        let refs = FileResolvedRefs::new("rust");
        store.write_resolved_hex(&hash_hex, &refs).expect("write resolved blob");

        let path = store.blob_path_rref_hex(&hash_hex);
        let mut persisted = std::fs::read(&path).expect("read resolved bytes");
        persisted[SINGLE_HEADER_LEN] ^= 0xff;
        std::fs::write(&path, persisted).expect("corrupt compressed payload");

        store.write_resolved_hex(&hash_hex, &refs).expect("repair corrupt blob");
        assert_eq!(store.read_resolved_by_hex(&hash_hex).unwrap(), Some(refs));
    }

    #[cfg(feature = "code-search")]
    #[test]
    fn legacy_uncompressed_chunk_blobs_remain_readable_and_peekable() {
        init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash_hex = "4".repeat(64);
        let blob = crate::chunk::CodeChunkBlob {
            schema_ver: SCHEMA_VER,
            embedding_dim: 768,
            embedding_model: "balanced".to_string(),
            chunks: Vec::new(),
            embeddings: Vec::new(),
        };
        let legacy = rmp_serde::to_vec_named(&blob).expect("serialize legacy chunk blob");
        std::fs::write(store.blob_path_chunk_hex(&hash_hex), legacy).expect("write legacy chunk blob");

        assert_eq!(store.read_chunks_by_hex(&hash_hex).unwrap(), Some(blob));
        let peek = store.peek_chunk_state(&hash_hex).unwrap().expect("legacy chunk peek");
        assert_eq!(peek.embedding_dim, 768);
        assert_eq!(peek.embedding_model, "balanced");
        assert_eq!(peek.chunks.len(), 0);
        assert_eq!(peek.embeddings.len(), 0);
    }

    #[cfg(feature = "code-search")]
    #[test]
    fn chunk_peek_reads_plain_metadata_without_decompressing_payload() {
        init_isolated_cache();
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path(), VIEW_WORKING).expect("open store");
        let hash_hex = "3".repeat(64);
        let blob = crate::chunk::CodeChunkBlob {
            schema_ver: SCHEMA_VER,
            embedding_dim: 768,
            embedding_model: "balanced".to_string(),
            chunks: Vec::new(),
            embeddings: Vec::new(),
        };
        store.write_chunks_hex(&hash_hex, &blob).expect("write chunk blob");

        let path = store.blob_path_chunk_hex(&hash_hex);
        let mut persisted = std::fs::read(&path).expect("read chunk blob bytes");
        *persisted.last_mut().expect("compressed payload present") ^= 0xff;
        std::fs::write(&path, persisted).expect("corrupt compressed chunk payload");

        let peek = store
            .peek_chunk_state(&hash_hex)
            .expect("plain chunk metadata remains readable")
            .expect("chunk peek present");
        assert_eq!(peek.embedding_dim, 768);
        assert_eq!(peek.embedding_model, "balanced");
        assert_eq!(peek.chunks.len(), 0);
        assert_eq!(peek.embeddings.len(), 0);
        assert!(
            store.read_chunks_by_hex(&hash_hex).is_err(),
            "full read must observe corruption"
        );
    }
}
