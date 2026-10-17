//! Byte-level key encoding for the **import-probe** Fjall keyspaces that make the incremental
//! cross-file re-stitch exact and O(affected): `import_targets` and `importers_by_probe`.
//!
//! - `import_targets` records the stitch result per import binding:
//!   `u16:len(importer) ‖ importer ‖ local_start:u32_be  →  u16:len(def_path) ‖ def_path ‖ def_start:u32_be`.
//!   The reserved [`PROBE_MANIFEST_START`] row of an importer carries its probe manifest instead (the
//!   concatenated [`ProbeHash`]es of every `importers_by_probe` row it owns), so a re-stitch can
//!   delete exactly the rows it wrote without a third keyspace.
//! - `importers_by_probe` is the reverse map of "which importers consulted this path":
//!   `probe_hash:8 ‖ importer`, with an empty value. A prefix scan on the 8-byte hash yields every
//!   importer whose resolution touched the path (a hit target, a re-export hop file, a failed
//!   candidate, or a `tsconfig.json` / `package.json` it read).
//!
//! The probed path is hashed (blake3, first 8 bytes) rather than stored: the row count is one per
//! `(importer, probed path)` pair, and a collision only widens the affected set, never narrows it.
//!
//! Public paths are unchanged — [`crate::index::keys`] re-exports every item below.

use super::keys::{read_len_prefixed_ref, write_len_prefixed};
use crate::path::RelPath;

/// Width of a [`ProbeHash`] in bytes.
pub const PROBE_HASH_LEN: usize = 8;

/// The truncated blake3 digest of a probed repo-relative path.
pub type ProbeHash = [u8; PROBE_HASH_LEN];

/// `local_start` value of the reserved manifest row in `import_targets`. A real binding offset is a
/// byte position in a source file, so `u32::MAX` (4 GiB) cannot collide with one.
pub const PROBE_MANIFEST_START: u32 = u32::MAX;

/// Hash a probed repo-relative, forward-slashed path.
pub fn probe_hash(path: &str) -> ProbeHash {
    let digest = blake3::hash(path.as_bytes());
    let mut out = [0u8; PROBE_HASH_LEN];
    out.copy_from_slice(&digest.as_bytes()[..PROBE_HASH_LEN]);
    out
}

/// `import_targets` key: `u16:len(importer) ‖ importer ‖ local_start:u32_be`.
pub fn import_target(importer: &RelPath, local_start: u32) -> Vec<u8> {
    let mut out = import_targets_prefix(importer);
    out.extend_from_slice(&local_start.to_be_bytes());
    out
}

/// Prefix bytes for "every `import_targets` row of this importer" (bindings plus manifest).
pub fn import_targets_prefix(importer: &RelPath) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + importer.as_bytes().len() + 4);
    let _ = write_len_prefixed(&mut out, importer.as_bytes());
    out
}

/// `import_targets` value: `u16:len(def_path) ‖ def_path ‖ def_start:u32_be`.
pub fn import_target_value(def_path: &RelPath, def_start: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + def_path.as_bytes().len() + 4);
    let _ = write_len_prefixed(&mut out, def_path.as_bytes());
    out.extend_from_slice(&def_start.to_be_bytes());
    out
}

/// Decode an `import_targets` binding row to `(importer, local_start, def_path, def_start)`. `None`
/// for the manifest row and for malformed input.
pub fn parse_import_target(key: &[u8], value: &[u8]) -> Option<(RelPath, u32, RelPath, u32)> {
    let mut c = 0;
    let importer = read_len_prefixed_ref(key, &mut c)?;
    let local_start = u32::from_be_bytes(key.get(c..c + 4)?.try_into().ok()?);
    if local_start == PROBE_MANIFEST_START {
        return None;
    }
    let mut v = 0;
    let def_path = read_len_prefixed_ref(value, &mut v)?;
    let def_start = u32::from_be_bytes(value.get(v..v + 4)?.try_into().ok()?);
    Some((RelPath::from(importer), local_start, RelPath::from(def_path), def_start))
}

/// Encode the manifest value: the probe hashes back to back.
pub fn probe_manifest_value(hashes: &[ProbeHash]) -> Vec<u8> {
    hashes.concat()
}

/// Decode a manifest value back into its hashes (a trailing partial hash is ignored).
pub fn parse_probe_manifest(value: &[u8]) -> impl Iterator<Item = ProbeHash> + '_ {
    value
        .chunks_exact(PROBE_HASH_LEN)
        .filter_map(|chunk| ProbeHash::try_from(chunk).ok())
}

/// `importers_by_probe` key: `probe_hash:8 ‖ importer`.
pub fn importer_by_probe(hash: &ProbeHash, importer: &RelPath) -> Vec<u8> {
    let mut out = Vec::with_capacity(PROBE_HASH_LEN + importer.as_bytes().len());
    out.extend_from_slice(hash);
    out.extend_from_slice(importer.as_bytes());
    out
}

/// Prefix bytes for "every importer that probed the path with this hash".
pub fn importers_by_probe_prefix(hash: &ProbeHash) -> Vec<u8> {
    hash.to_vec()
}

/// The importer of an `importers_by_probe` key.
pub fn parse_importer_by_probe(key: &[u8]) -> Option<RelPath> {
    key.get(PROBE_HASH_LEN..)
        .filter(|rest| !rest.is_empty())
        .map(RelPath::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_target_roundtrips() {
        let importer = RelPath::from("src/app.ts");
        let def = RelPath::from("src/impl.ts");
        let key = import_target(&importer, 12);
        let value = import_target_value(&def, 340);
        assert_eq!(parse_import_target(&key, &value), Some((importer, 12, def, 340)));
    }

    #[test]
    fn manifest_row_is_not_a_binding_and_shares_the_importer_prefix() {
        let importer = RelPath::from("src/app.ts");
        let manifest_key = import_target(&importer, PROBE_MANIFEST_START);
        assert!(manifest_key.starts_with(&import_targets_prefix(&importer)));
        assert_eq!(
            parse_import_target(&manifest_key, &import_target_value(&importer, 0)),
            None
        );
    }

    #[test]
    fn import_targets_prefix_isolates_importers() {
        let a = RelPath::from("src/a.ts");
        let ab = RelPath::from("src/a.ts.bak");
        assert!(!import_target(&ab, 1).starts_with(&import_targets_prefix(&a)));
    }

    #[test]
    fn manifest_roundtrips() {
        let hashes = [probe_hash("a/b.ts"), probe_hash("tsconfig.json")];
        let value = probe_manifest_value(&hashes);
        assert_eq!(parse_probe_manifest(&value).collect::<Vec<_>>(), hashes);
    }

    #[test]
    fn importers_by_probe_roundtrips_and_isolates_probes() {
        let importer = RelPath::from("src/app.ts");
        let hash = probe_hash("src/impl.ts");
        let key = importer_by_probe(&hash, &importer);
        assert_eq!(parse_importer_by_probe(&key), Some(importer.clone()));
        assert!(key.starts_with(&importers_by_probe_prefix(&hash)));
        assert!(!key.starts_with(&importers_by_probe_prefix(&probe_hash("src/other.ts"))));
    }

    #[test]
    fn probe_hash_is_stable_and_path_sensitive() {
        assert_eq!(probe_hash("a.ts"), probe_hash("a.ts"));
        assert_ne!(probe_hash("a.ts"), probe_hash("b.ts"));
    }
}
