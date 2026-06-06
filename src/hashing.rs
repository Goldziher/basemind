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

pub fn from_hex(s: &str) -> Option<Hash> {
    let bytes = hex::decode(s).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Some(out)
}
