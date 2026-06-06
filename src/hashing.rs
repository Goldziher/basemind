use std::path::Path;

use blake3::Hasher;

pub type Hash = [u8; 32];

pub fn hash_bytes(bytes: &[u8]) -> Hash {
    let mut h = Hasher::new();
    h.update(bytes);
    *h.finalize().as_bytes()
}

pub fn hash_file(path: &Path) -> std::io::Result<Hash> {
    let bytes = std::fs::read(path)?;
    Ok(hash_bytes(&bytes))
}

pub fn hex(h: &Hash) -> String {
    hex::encode(h)
}

/// Zero-alloc hex encode into a stack buffer. `hash_hex_str(&buf)` views it as `&str`.
pub fn hex_buf(h: &Hash) -> [u8; 64] {
    let mut out = [0u8; 64];
    hex::encode_to_slice(h, &mut out).expect("32 bytes -> 64 hex chars");
    out
}

/// Borrow the stack hex buffer as `&str`. SAFETY: contents are always lowercase ASCII hex.
pub fn hex_str(buf: &[u8; 64]) -> &str {
    // The buffer is filled exclusively by `hex::encode_to_slice` which only emits ASCII.
    unsafe { std::str::from_utf8_unchecked(buf) }
}

/// Decode a 64-char hex string straight into a `[u8;32]` — no Vec alloc.
pub fn from_hex(s: &str) -> Option<Hash> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    hex::decode_to_slice(s, &mut out).ok()?;
    Some(out)
}
