//! Shared primitives for the compressed content-addressed blob envelope.

use std::path::Path;

use crate::store::StoreError;

const BLOB_MAGIC: &[u8; 4] = b"BMB1";
const ENVELOPE_PREFIX_LEN: usize = 8;
const ZSTD_CODEC: u8 = 1;
const ZSTD_LEVEL: i32 = 1;
const MAX_DECOMPRESSED_BLOB_BYTES: usize = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum BlobKind {
    Single = 1,
    FileMap = 2,
    Chunk = 3,
}

impl BlobKind {
    fn parse(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Single),
            2 => Some(Self::FileMap),
            3 => Some(Self::Chunk),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct EnvelopePrefix {
    pub(crate) kind: BlobKind,
    pub(crate) schema_ver: u16,
}

pub(crate) struct CompressedPayload<'a> {
    pub(crate) uncompressed_len: usize,
    pub(crate) bytes: &'a [u8],
}

pub(crate) fn corrupt_blob(path: &Path) -> StoreError {
    StoreError::CorruptBlob {
        path: path.to_path_buf(),
    }
}

pub(crate) fn envelope_prefix(path: &Path, bytes: &[u8]) -> Result<Option<EnvelopePrefix>, StoreError> {
    if !bytes.starts_with(BLOB_MAGIC) {
        return Ok(None);
    }
    let prefix = bytes.get(..ENVELOPE_PREFIX_LEN).ok_or_else(|| corrupt_blob(path))?;
    if prefix[4] != ZSTD_CODEC {
        return Err(corrupt_blob(path));
    }
    let kind = BlobKind::parse(prefix[5]).ok_or_else(|| corrupt_blob(path))?;
    let schema_ver = u16::from_le_bytes([prefix[6], prefix[7]]);
    Ok(Some(EnvelopePrefix { kind, schema_ver }))
}

#[cfg(feature = "code-search")]
pub(crate) fn read_u16(path: &Path, bytes: &[u8], offset: usize) -> Result<u16, StoreError> {
    let raw: [u8; 2] = bytes
        .get(offset..offset + 2)
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| corrupt_blob(path))?;
    Ok(u16::from_le_bytes(raw))
}

pub(crate) fn read_u32(path: &Path, bytes: &[u8], offset: usize) -> Result<usize, StoreError> {
    let raw: [u8; 4] = bytes
        .get(offset..offset + 4)
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| corrupt_blob(path))?;
    Ok(u32::from_le_bytes(raw) as usize)
}

pub(crate) fn checked_u32_len(len: usize) -> Result<u32, StoreError> {
    u32::try_from(len).map_err(|_| StoreError::BlobTooLarge)
}

pub(crate) fn compress_payload(bytes: &[u8]) -> Result<Vec<u8>, StoreError> {
    zstd::bulk::compress(bytes, ZSTD_LEVEL).map_err(StoreError::Compression)
}

pub(crate) fn decompress_payload(path: &Path, payload: CompressedPayload<'_>) -> Result<Vec<u8>, StoreError> {
    if payload.uncompressed_len > MAX_DECOMPRESSED_BLOB_BYTES {
        return Err(corrupt_blob(path));
    }
    let decoded = zstd::bulk::decompress(payload.bytes, payload.uncompressed_len).map_err(|source| {
        StoreError::Decompression {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if decoded.len() != payload.uncompressed_len {
        return Err(corrupt_blob(path));
    }
    Ok(decoded)
}

pub(crate) fn encode_prefix(kind: BlobKind, schema_ver: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(BLOB_MAGIC);
    out.push(ZSTD_CODEC);
    out.push(kind as u8);
    out.extend_from_slice(&schema_ver.to_le_bytes());
}
