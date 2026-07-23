//! Blob (de)framing + atomic write for the content-addressed extraction store, and the
//! [`Store`] accessors layered over them.
//!
//! Each indexed source file persists one combined-filemap blob `<hash>.fm.msgpack`, framed
//! `[l1_len: u32 LE][l1 msgpack][l2 msgpack | empty]` — the L1 outline and (when extracted
//! eagerly) the L2 calls in a single content-addressed file. Fusing the two tiers halves the
//! per-file blob writes (`open` + atomic `rename`) on the default eager-L2 scan; the
//! length-prefix lets the common outline-only read decode just the L1 slice without touching
//! L2. The doc tier (`write_blob`) stays a plain unframed msgpack blob.
//!
//! The per-tier `Store::{blob_path,read,write}_*` methods moved here from `store.rs` (which was
//! over the 1000-line module cap): they are the blob store's read/write surface — one tier per
//! blob suffix (`.fm` / `.doc` / `.rref` / `.chunk`) — and change for the same reason the framing
//! does. They stay inherent methods on [`Store`], so every call site is unaffected by the move.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::extract::SCHEMA_VER;
use crate::extract::{FileMapL1, FileMapL2};
use crate::hashing::{self, Hash};
use crate::store::{Store, StoreError, check_schema};

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

/// Plain (unframed) msgpack blob peek: read only the leading `schema_ver` field. Shared by the
/// doc tier and the resolution tier (both are unframed single-map blobs).
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

/// Unframed single-map blob write (doc tier + resolution tier): content-addressed skip on
/// matching schema, else serialize + atomic write. The combined-filemap blobs go through
/// `Store::write_filemap_hex` instead.
pub(crate) fn write_blob<T: serde::Serialize>(path: PathBuf, value: &T) -> Result<(), StoreError> {
    if path.exists() && peek_blob_schema(&path) == Some(SCHEMA_VER) {
        return Ok(());
    }
    let bytes = rmp_serde::to_vec_named(value)?;
    write_bytes_atomic(path, &bytes)
}

/// Like [`write_blob`] but always (re)writes, even when a same-schema blob already exists. The
/// embedding payload of a chunk sidecar or a doc blob can change for the SAME content hash — a
/// vectorless `Deferred` blob (`embedding_dim: 0`) later upgraded to an embedded `Inline` blob — so
/// a schema-only skip would wrongly keep the unembedded blob and vector search would serve nothing.
#[cfg(any(feature = "code-search", feature = "documents"))]
pub(crate) fn write_blob_overwrite<T: serde::Serialize>(path: PathBuf, value: &T) -> Result<(), StoreError> {
    let bytes = rmp_serde::to_vec_named(value)?;
    write_bytes_atomic(path, &bytes)
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

    /// Write the combined-filemap blob for a file. Holds both tiers in one content-addressed
    /// blob (`[l1_len][l1][l2|empty]`), so the default eager-L2 scan does one `open` + `write`
    /// + atomic `rename` per file instead of two. `l2 = None` writes an L1-only frame.
    pub fn write_filemap_hex(&self, hash_hex: &str, l1: &FileMapL1, l2: Option<&FileMapL2>) -> Result<(), StoreError> {
        let path = self.blob_path_fm_hex(hash_hex);
        if path.exists() && peek_filemap_schema(&path) == Some(SCHEMA_VER) {
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

    /// Path of a file's resolution blob (`<hash>.rref.msgpack`) — the per-file code-intelligence
    /// facts (intra-file resolved edges + import/export list). A sibling of the `.fm`/`.doc`
    /// blobs, content-addressed by source hash. Unframed single-map msgpack, like the doc tier.
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
        let refs: crate::intel::model::FileResolvedRefs = rmp_serde::from_slice(&bytes)?;
        check_schema(refs.schema_ver)?;
        Ok(Some(refs))
    }

    /// Path of a file's code-chunk sidecar (`<hash>.chunk.msgpack`) — the per-file chunk list +
    /// embeddings that back the semantic code-search tier. A sibling of the `.fm`/`.doc`/`.rref`
    /// blobs, content-addressed by source hash. Unframed single-map msgpack, like the doc tier.
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
        write_blob_overwrite(self.blob_path_chunk_hex(hash_hex), blob)
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
        let blob: crate::chunk::CodeChunkBlob = rmp_serde::from_slice(&bytes)?;
        check_schema(blob.schema_ver)?;
        Ok(Some(blob))
    }

    /// Cheaply read a chunk sidecar's embedding state without decoding the chunk text. Same contract
    /// as [`read_chunks_by_hex`](Self::read_chunks_by_hex) — `Ok(None)` when the file has no chunk
    /// blob, a schema mismatch surfaces as an error — but decodes only the counts + embedding
    /// dim/model via [`CodeChunkBlobPeek`](crate::chunk::CodeChunkBlobPeek), skipping the heavy
    /// chunk/embedding element contents. Backs the `embed_state_satisfied` unchanged-file fast path.
    #[cfg(feature = "code-search")]
    pub fn peek_chunk_state(&self, hash_hex: &str) -> Result<Option<crate::chunk::CodeChunkBlobPeek>, StoreError> {
        let path = self.blob_path_chunk_hex(hash_hex);
        let Some(bytes) = read_if_exists(&path)? else {
            return Ok(None);
        };
        let peek: crate::chunk::CodeChunkBlobPeek = rmp_serde::from_slice(&bytes)?;
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
        let read = store.read_resolved_by_hex(&hash_hex).expect("read resolved blob");
        assert_eq!(read.as_ref(), Some(&refs), "resolution blob round-trips exactly");

        let missing = store.read_resolved_by_hex(&"e".repeat(64)).expect("read missing");
        assert_eq!(missing, None, "absent resolution blob reads back as None");
    }
}
