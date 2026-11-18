//! Byte-level key encoding/decoding for the Fjall inverted index.
//!
//! Each function encodes a primary key for one partition. Companion `parse_*` functions
//! decode the components back so the reader path can reconstruct `(rel_path, byte offset)`
//! from a raw key buffer.
//!
//! All length-prefixed components use `u16` big-endian — paths and identifiers in real code
//! are far below 64 KiB. Byte offsets in source files use `u32` big-endian. Big-endian
//! orderings keep prefix-scan semantics intuitive: a `range("foo\0".."foo\0\xff")` over
//! `calls_by_callee` returns exactly the hits for callee `"foo"`.
//!
//! This module holds the **code-map** keyspaces — every key here is addressed by a
//! `(rel_path, byte offset)` position in the scanned tree. The agent-memory keyspace, which is
//! addressed by a `(scope, ordinal, owner)` namespace instead, lives in
//! [`crate::index::keys_memory`] and is re-exported below so the `crate::index::keys::*` paths
//! stay stable.

use crate::extract::SymbolKind;
use crate::path::RelPath;

pub use crate::index::keys_bm25::{
    BM25_KEY_LEN, Bm25Key, POSTING_ENTRY_LEN, bm25_content_key, code_bm25_by_path, code_bm25_by_path_prefix,
    code_bm25_by_path_value, code_bm25_holder, code_bm25_posting, code_bm25_posting_entries, code_bm25_posting_value,
    code_bm25_postings_prefix, parse_code_bm25_by_path, parse_code_bm25_by_path_value, parse_code_bm25_posting,
    parse_code_bm25_posting_content, split_chunk_id,
};
pub use crate::index::keys_content::{
    CONTENT_KEY_LEN, ContentKey, NameRow, call_by_callee, call_by_callee_content, call_by_callee_hit, call_name_row,
    calls_by_callee_prefix, content_key, content_row, pack_starts, parse_call_by_callee, parse_content_row,
    parse_symbol_by_name, parse_symbol_row, split_hit, symbol_by_name, symbol_by_name_hit, symbols_by_name_prefix,
    unpack_starts,
};
pub use crate::index::keys_memory::{
    MEMORY_VIS_GROUP, MEMORY_VIS_INDIVIDUAL, memory_by_key, memory_by_key_ns_prefix, parse_memory_by_key,
    parse_memory_key_only,
};
pub use crate::index::keys_probe::{
    PROBE_HASH_LEN, PROBE_MANIFEST_START, ProbeHash, import_target, import_target_value, import_targets_prefix,
    importer_by_probe, importers_by_probe_prefix, parse_import_target, parse_importer_by_probe, parse_probe_manifest,
    probe_hash, probe_manifest_value,
};

/// `u16:name_len ‖ name`. Internal helper.
///
/// Returns `None` when `bytes` exceeds 65535 bytes (the u16 ceiling). Path encoders that
/// call this for `RelPath` components may ignore the return value with `let _ = …` — real
/// file paths never hit 64 KiB. Identifier encoders (`symbol_by_name`, `call_by_callee`,
/// `import_by_module`, `import_by_path`, `impl_by_trait`, `impl_by_path`) return `Option`
/// and propagate `None` to their callers so that pathologically long tokens are silently
/// skipped rather than panicking inside a rayon `par_iter`.
pub(super) fn write_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Option<()> {
    let len = u16::try_from(bytes.len()).ok()?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Some(())
}

pub(super) fn read_len_prefixed(buf: &[u8], cursor: &mut usize) -> Option<Vec<u8>> {
    if buf.len() < *cursor + 2 {
        return None;
    }
    let len = u16::from_be_bytes([buf[*cursor], buf[*cursor + 1]]) as usize;
    *cursor += 2;
    if buf.len() < *cursor + len {
        return None;
    }
    let out = buf[*cursor..*cursor + len].to_vec();
    *cursor += len;
    Some(out)
}

/// Zero-copy variant of `read_len_prefixed` — returns a borrowed slice into `buf` instead
/// of allocating a `Vec<u8>`. Use this on the parse path when the next consumer (e.g.
/// `RelPath::from(&[u8])`) copies the bytes internally; the intermediate `Vec` would be
/// a wasted allocation.
pub(super) fn read_len_prefixed_ref<'buf>(buf: &'buf [u8], cursor: &mut usize) -> Option<&'buf [u8]> {
    if buf.len() < *cursor + 2 {
        return None;
    }
    let len = u16::from_be_bytes([buf[*cursor], buf[*cursor + 1]]) as usize;
    *cursor += 2;
    if buf.len() < *cursor + len {
        return None;
    }
    let out = &buf[*cursor..*cursor + len];
    *cursor += len;
    Some(out)
}

/// `imports_by_module`: `u16:len(module) ‖ module ‖ u16:len(rel) ‖ rel ‖ start_byte:u32_be`.
///
/// Returns `None` when `module` exceeds 65535 bytes. The caller skips the secondary-index
/// entry but still writes the primary `imports_by_path` entry so the import record stays complete.
pub fn import_by_module(module: &str, rel: &RelPath, start_byte: u32) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(2 + module.len() + 2 + rel.as_bytes().len() + 4);
    write_len_prefixed(&mut out, module.as_bytes())?;
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
    out.extend_from_slice(&start_byte.to_be_bytes());
    Some(out)
}

pub fn imports_by_module_prefix(module: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + module.len());
    let _ = write_len_prefixed(&mut out, module.as_bytes());
    out
}

pub fn parse_import_by_module(key: &[u8]) -> Option<(String, RelPath, u32)> {
    let mut c = 0;
    let module = String::from_utf8(read_len_prefixed(key, &mut c)?).ok()?;
    let rel = read_len_prefixed_ref(key, &mut c)?;
    if key.len() < c + 4 {
        return None;
    }
    let start = u32::from_be_bytes([key[c], key[c + 1], key[c + 2], key[c + 3]]);
    Some((module, RelPath::from(rel), start))
}

/// `imports_by_path`: same role as `symbols_by_path` for the imports keyspace —
/// gives O(prefix) deletion when re-upserting a file. Shape:
/// `u16:len(rel) ‖ rel ‖ u16:len(module) ‖ module ‖ start_byte:u32_be`.
///
/// Returns `None` when `module` exceeds 65535 bytes. The rel component is path-only
/// and never reaches the 64 KiB ceiling.
pub fn import_by_path(rel: &RelPath, module: &str, start_byte: u32) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len() + 2 + module.len() + 4);
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
    write_len_prefixed(&mut out, module.as_bytes())?;
    out.extend_from_slice(&start_byte.to_be_bytes());
    Some(out)
}

pub fn imports_by_path_prefix(rel: &RelPath) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len());
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
    out
}

pub fn parse_import_by_path(key: &[u8]) -> Option<(RelPath, String, u32)> {
    let mut c = 0;
    let rel = read_len_prefixed_ref(key, &mut c)?;
    let module = String::from_utf8(read_len_prefixed(key, &mut c)?).ok()?;
    if key.len() < c + 4 {
        return None;
    }
    let start = u32::from_be_bytes([key[c], key[c + 1], key[c + 2], key[c + 3]]);
    Some((RelPath::from(rel), module, start))
}

/// `implementations_by_trait`: prefix-scan keyspace for `find_implementations`. Shape:
/// `u16:len(trait_name) ‖ trait_name ‖ u16:len(impl_type) ‖ impl_type ‖
/// u16:len(rel) ‖ rel ‖ start_byte:u32_be`.
///
/// Returns `None` when `trait_name` or `impl_type` exceeds 65535 bytes. The caller skips
/// the secondary-index entry but still writes the primary `implementations_by_path` entry.
pub fn impl_by_trait(trait_name: &str, impl_type: &str, rel: &RelPath, start_byte: u32) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(2 + trait_name.len() + 2 + impl_type.len() + 2 + rel.as_bytes().len() + 4);
    write_len_prefixed(&mut out, trait_name.as_bytes())?;
    write_len_prefixed(&mut out, impl_type.as_bytes())?;
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
    out.extend_from_slice(&start_byte.to_be_bytes());
    Some(out)
}

pub fn impls_by_trait_prefix(trait_name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + trait_name.len());
    let _ = write_len_prefixed(&mut out, trait_name.as_bytes());
    out
}

pub fn parse_impl_by_trait(key: &[u8]) -> Option<(String, String, RelPath, u32)> {
    let mut c = 0;
    let trait_name = String::from_utf8(read_len_prefixed(key, &mut c)?).ok()?;
    let impl_type = String::from_utf8(read_len_prefixed(key, &mut c)?).ok()?;
    let rel = read_len_prefixed_ref(key, &mut c)?;
    if key.len() < c + 4 {
        return None;
    }
    let start = u32::from_be_bytes([key[c], key[c + 1], key[c + 2], key[c + 3]]);
    Some((trait_name, impl_type, RelPath::from(rel), start))
}

/// `implementations_by_path`: companion partition keyed by file so the per-file delete on
/// upsert is O(prefix) instead of a full-iter scan. Shape:
/// `u16:len(rel) ‖ rel ‖ u16:len(trait_name) ‖ trait_name ‖
/// u16:len(impl_type) ‖ impl_type ‖ start_byte:u32_be`.
///
/// Returns `None` when `trait_name` or `impl_type` exceeds 65535 bytes. The rel component
/// is path-only and never reaches the 64 KiB ceiling.
pub fn impl_by_path(rel: &RelPath, trait_name: &str, impl_type: &str, start_byte: u32) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len() + 2 + trait_name.len() + 2 + impl_type.len() + 4);
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
    write_len_prefixed(&mut out, trait_name.as_bytes())?;
    write_len_prefixed(&mut out, impl_type.as_bytes())?;
    out.extend_from_slice(&start_byte.to_be_bytes());
    Some(out)
}

pub fn impls_by_path_prefix(rel: &RelPath) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len());
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
    out
}

pub fn parse_impl_by_path(key: &[u8]) -> Option<(RelPath, String, String, u32)> {
    let mut c = 0;
    let rel = read_len_prefixed_ref(key, &mut c)?;
    let trait_name = String::from_utf8(read_len_prefixed(key, &mut c)?).ok()?;
    let impl_type = String::from_utf8(read_len_prefixed(key, &mut c)?).ok()?;
    if key.len() < c + 4 {
        return None;
    }
    let start = u32::from_be_bytes([key[c], key[c + 1], key[c + 2], key[c + 3]]);
    Some((RelPath::from(rel), trait_name, impl_type, start))
}

/// `refs_by_def`: resolved "references to a definition", prefix-scannable by the defining site.
/// Shape:
/// `u16:len(def_path) ‖ def_path ‖ def_start:u32_be ‖ u16:len(use_path) ‖ use_path ‖ use_start:u32_be`.
///
/// A [`refs_by_def_prefix`] range scan over `(def_path, def_start)` returns every use resolved to
/// that definition — the scope/import-resolved backing for `find_references` / `find_callers`.
/// Both endpoints are byte offsets; the def and use paths may differ (cross-file edge).
pub fn ref_by_def(def_path: &RelPath, def_start: u32, use_path: &RelPath, use_start: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + def_path.as_bytes().len() + 4 + 2 + use_path.as_bytes().len() + 4);
    let _ = write_len_prefixed(&mut out, def_path.as_bytes());
    out.extend_from_slice(&def_start.to_be_bytes());
    let _ = write_len_prefixed(&mut out, use_path.as_bytes());
    out.extend_from_slice(&use_start.to_be_bytes());
    out
}

/// Prefix bytes for "all references to the definition at `(def_path, def_start)`".
pub fn refs_by_def_prefix(def_path: &RelPath, def_start: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + def_path.as_bytes().len() + 4);
    let _ = write_len_prefixed(&mut out, def_path.as_bytes());
    out.extend_from_slice(&def_start.to_be_bytes());
    out
}

pub fn parse_ref_by_def(key: &[u8]) -> Option<(RelPath, u32, RelPath, u32)> {
    let mut c = 0;
    let def_path = read_len_prefixed_ref(key, &mut c)?;
    if key.len() < c + 4 {
        return None;
    }
    let def_start = u32::from_be_bytes([key[c], key[c + 1], key[c + 2], key[c + 3]]);
    c += 4;
    let use_path = read_len_prefixed_ref(key, &mut c)?;
    if key.len() < c + 4 {
        return None;
    }
    let use_start = u32::from_be_bytes([key[c], key[c + 1], key[c + 2], key[c + 3]]);
    Some((RelPath::from(def_path), def_start, RelPath::from(use_path), use_start))
}

/// `refs_by_path`: companion keyed by the USE file. Serves two roles — O(prefix) delete of a
/// file's resolved edges on re-resolve, and the forward lookup that backs `goto_definition`
/// (a use position → its definition). Shape:
/// `u16:len(use_path) ‖ use_path ‖ use_start:u32_be ‖ u16:len(def_path) ‖ def_path ‖ def_start:u32_be`.
pub fn ref_by_path(use_path: &RelPath, use_start: u32, def_path: &RelPath, def_start: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + use_path.as_bytes().len() + 4 + 2 + def_path.as_bytes().len() + 4);
    let _ = write_len_prefixed(&mut out, use_path.as_bytes());
    out.extend_from_slice(&use_start.to_be_bytes());
    let _ = write_len_prefixed(&mut out, def_path.as_bytes());
    out.extend_from_slice(&def_start.to_be_bytes());
    out
}

/// Prefix bytes for "every resolved edge whose use is in this file" — used for the per-file
/// delete on re-resolve.
pub fn refs_by_path_prefix(use_path: &RelPath) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + use_path.as_bytes().len());
    let _ = write_len_prefixed(&mut out, use_path.as_bytes());
    out
}

/// Prefix bytes for "the definition of the use at `(use_path, use_start)`" — backs `goto_definition`.
pub fn refs_by_use_prefix(use_path: &RelPath, use_start: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + use_path.as_bytes().len() + 4);
    let _ = write_len_prefixed(&mut out, use_path.as_bytes());
    out.extend_from_slice(&use_start.to_be_bytes());
    out
}

pub fn parse_ref_by_path(key: &[u8]) -> Option<(RelPath, u32, RelPath, u32)> {
    let mut c = 0;
    let use_path = read_len_prefixed_ref(key, &mut c)?;
    if key.len() < c + 4 {
        return None;
    }
    let use_start = u32::from_be_bytes([key[c], key[c + 1], key[c + 2], key[c + 3]]);
    c += 4;
    let def_path = read_len_prefixed_ref(key, &mut c)?;
    if key.len() < c + 4 {
        return None;
    }
    let def_start = u32::from_be_bytes([key[c], key[c + 1], key[c + 2], key[c + 3]]);
    Some((RelPath::from(use_path), use_start, RelPath::from(def_path), def_start))
}

/// One-byte ordinal for a `SymbolKind`. Stable across releases so existing keys stay valid;
/// new variants extend the tail. Keep the explicit assignments — accidentally reordering
/// would silently miscategorize cached entries.
pub(super) fn symbol_kind_byte(k: SymbolKind) -> u8 {
    match k {
        SymbolKind::Unknown => 0,
        SymbolKind::Function => 1,
        SymbolKind::Method => 2,
        SymbolKind::Struct => 3,
        SymbolKind::Enum => 4,
        SymbolKind::Class => 5,
        SymbolKind::Interface => 6,
        SymbolKind::Trait => 7,
        SymbolKind::Type => 8,
        SymbolKind::Const => 9,
        SymbolKind::Module => 10,
        SymbolKind::Macro => 11,
        SymbolKind::Impl => 12,
        SymbolKind::Namespace => 13,
        SymbolKind::Getter => 14,
        SymbolKind::Setter => 15,
        SymbolKind::Field => 16,
        SymbolKind::Variable => 17,
        SymbolKind::EnumVariant => 18,
        SymbolKind::Constructor => 19,
        SymbolKind::Decorator => 20,
        SymbolKind::Heading => 21,
    }
}

pub(super) fn symbol_kind_from_byte(b: u8) -> SymbolKind {
    match b {
        1 => SymbolKind::Function,
        2 => SymbolKind::Method,
        3 => SymbolKind::Struct,
        4 => SymbolKind::Enum,
        5 => SymbolKind::Class,
        6 => SymbolKind::Interface,
        7 => SymbolKind::Trait,
        8 => SymbolKind::Type,
        9 => SymbolKind::Const,
        10 => SymbolKind::Module,
        11 => SymbolKind::Macro,
        12 => SymbolKind::Impl,
        13 => SymbolKind::Namespace,
        14 => SymbolKind::Getter,
        15 => SymbolKind::Setter,
        16 => SymbolKind::Field,
        17 => SymbolKind::Variable,
        18 => SymbolKind::EnumVariant,
        19 => SymbolKind::Constructor,
        20 => SymbolKind::Decorator,
        21 => SymbolKind::Heading,
        _ => SymbolKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_by_module_roundtrips() {
        let rel = RelPath::from("src/foo.py");
        let key = import_by_module("os.path", &rel, 0).unwrap();
        let (module, back, start) = parse_import_by_module(&key).unwrap();
        assert_eq!(module, "os.path");
        assert_eq!(back, rel);
        assert_eq!(start, 0);
    }

    #[test]
    fn import_by_path_roundtrips() {
        let rel = RelPath::from("src/foo.py");
        let key = import_by_path(&rel, "os.path", 42).unwrap();
        let (back_rel, module, start) = parse_import_by_path(&key).unwrap();
        assert_eq!(back_rel, rel);
        assert_eq!(module, "os.path");
        assert_eq!(start, 42);
    }

    /// `imports_by_path` prefix scan must isolate one file's entries from another file
    /// whose path shares a leading substring (e.g. `src/foo.py` vs `src/foo.py.bak`).
    #[test]
    fn prefix_scan_isolates_imports_by_path() {
        let rel_a = RelPath::from("src/foo.py");
        let rel_b = RelPath::from("src/foo.py.bak");
        let key_a = import_by_path(&rel_a, "os", 0).unwrap();
        let key_b = import_by_path(&rel_b, "os", 0).unwrap();
        let prefix_a = imports_by_path_prefix(&rel_a);
        assert!(key_a.starts_with(&prefix_a), "rel_a's key must extend rel_a's prefix");
        assert!(
            !key_b.starts_with(&prefix_a),
            "rel_b's key must NOT match rel_a's prefix"
        );
    }

    #[test]
    fn impl_by_trait_roundtrips() {
        let rel = RelPath::from("src/foo.rs");
        let key = impl_by_trait("Display", "Foo", &rel, 42).unwrap();
        let (trait_name, impl_type, back_rel, start) = parse_impl_by_trait(&key).unwrap();
        assert_eq!(trait_name, "Display");
        assert_eq!(impl_type, "Foo");
        assert_eq!(back_rel, rel);
        assert_eq!(start, 42);
    }

    #[test]
    fn impl_by_path_roundtrips() {
        let rel = RelPath::from("src/foo.rs");
        let key = impl_by_path(&rel, "Display", "Foo", 42).unwrap();
        let (back_rel, trait_name, impl_type, start) = parse_impl_by_path(&key).unwrap();
        assert_eq!(back_rel, rel);
        assert_eq!(trait_name, "Display");
        assert_eq!(impl_type, "Foo");
        assert_eq!(start, 42);
    }

    /// Prefix scan for `Display` must not bleed into `DisplayFmt`.
    #[test]
    fn prefix_scan_isolates_impls_by_trait() {
        let rel = RelPath::from("a.rs");
        let key_a = impl_by_trait("Display", "Foo", &rel, 1).unwrap();
        let key_b = impl_by_trait("DisplayFmt", "Foo", &rel, 1).unwrap();
        let prefix = impls_by_trait_prefix("Display");
        assert!(
            key_a.starts_with(&prefix),
            "Display's key must extend the Display prefix"
        );
        assert!(
            !key_b.starts_with(&prefix),
            "DisplayFmt's key must NOT match the Display prefix"
        );
    }

    /// `impls_by_path` prefix scan must isolate one file's entries from another file whose
    /// path shares a leading substring (e.g. `src/foo.rs` vs `src/foo.rs.bak`).
    #[test]
    fn prefix_scan_isolates_impls_by_path() {
        let rel_a = RelPath::from("src/foo.rs");
        let rel_b = RelPath::from("src/foo.rs.bak");
        let key_a = impl_by_path(&rel_a, "Display", "Foo", 0).unwrap();
        let key_b = impl_by_path(&rel_b, "Display", "Foo", 0).unwrap();
        let prefix_a = impls_by_path_prefix(&rel_a);
        assert!(key_a.starts_with(&prefix_a), "rel_a's key must extend rel_a's prefix");
        assert!(
            !key_b.starts_with(&prefix_a),
            "rel_b's key must NOT match rel_a's prefix"
        );
    }

    #[test]
    fn ref_by_def_roundtrips() {
        let def = RelPath::from("src/util.ts");
        let usef = RelPath::from("src/app.ts");
        let key = ref_by_def(&def, 100, &usef, 250);
        let (back_def, def_start, back_use, use_start) = parse_ref_by_def(&key).unwrap();
        assert_eq!(back_def, def);
        assert_eq!(def_start, 100);
        assert_eq!(back_use, usef);
        assert_eq!(use_start, 250);
    }

    #[test]
    fn ref_by_path_roundtrips() {
        let usef = RelPath::from("src/app.ts");
        let def = RelPath::from("src/util.ts");
        let key = ref_by_path(&usef, 250, &def, 100);
        let (back_use, use_start, back_def, def_start) = parse_ref_by_path(&key).unwrap();
        assert_eq!(back_use, usef);
        assert_eq!(use_start, 250);
        assert_eq!(back_def, def);
        assert_eq!(def_start, 100);
    }

    /// A `refs_by_def` scan for the definition at `(path, 100)` must not bleed into a
    /// definition at `(path, 1000)` in the same file — the `u32` def offset disambiguates.
    #[test]
    fn refs_by_def_prefix_isolates_definitions() {
        let def = RelPath::from("src/util.ts");
        let usef = RelPath::from("src/app.ts");
        let key_100 = ref_by_def(&def, 100, &usef, 5);
        let key_1000 = ref_by_def(&def, 1000, &usef, 5);
        let prefix_100 = refs_by_def_prefix(&def, 100);
        assert!(
            key_100.starts_with(&prefix_100),
            "def@100's edge must extend the def@100 prefix"
        );
        assert!(
            !key_1000.starts_with(&prefix_100),
            "def@1000's edge must NOT match the def@100 prefix"
        );
    }

    /// A `refs_by_path` file-level prefix must isolate one use-file's edges from another whose
    /// path shares a leading substring; and the position-level prefix must pin one use site.
    #[test]
    fn refs_by_path_prefixes_isolate() {
        let use_a = RelPath::from("src/app.ts");
        let use_b = RelPath::from("src/app.ts.bak");
        let def = RelPath::from("src/util.ts");
        let key_a = ref_by_path(&use_a, 250, &def, 100);
        let key_b = ref_by_path(&use_b, 250, &def, 100);
        let file_prefix = refs_by_path_prefix(&use_a);
        assert!(
            key_a.starts_with(&file_prefix),
            "use_a's edge must extend use_a's file prefix"
        );
        assert!(
            !key_b.starts_with(&file_prefix),
            "use_b's edge must NOT match use_a's file prefix"
        );

        let use_prefix = refs_by_use_prefix(&use_a, 250);
        let key_other_pos = ref_by_path(&use_a, 9, &def, 100);
        assert!(
            key_a.starts_with(&use_prefix),
            "use@250 edge must extend the use@250 prefix"
        );
        assert!(
            !key_other_pos.starts_with(&use_prefix),
            "a different use offset must NOT match the use@250 prefix"
        );
    }

    #[test]
    fn non_utf8_path_keys_roundtrip() {
        let rel = RelPath::from(b"f\xffoo.rs".as_slice());
        let key = import_by_path(&rel, "m", 7).unwrap();
        let (back, _, _) = parse_import_by_path(&key).unwrap();
        assert_eq!(back.as_bytes(), rel.as_bytes());
    }

    #[test]
    fn symbol_kind_byte_roundtrip_all_variants() {
        let all = [
            SymbolKind::Unknown,
            SymbolKind::Function,
            SymbolKind::Method,
            SymbolKind::Struct,
            SymbolKind::Enum,
            SymbolKind::Class,
            SymbolKind::Interface,
            SymbolKind::Trait,
            SymbolKind::Type,
            SymbolKind::Const,
            SymbolKind::Module,
            SymbolKind::Macro,
            SymbolKind::Impl,
            SymbolKind::Namespace,
            SymbolKind::Getter,
            SymbolKind::Setter,
        ];
        for k in all {
            assert_eq!(symbol_kind_from_byte(symbol_kind_byte(k)), k);
        }
    }

    /// The identifier-encoding functions must return `None` at the 65536-byte boundary
    /// rather than panicking. This protects the rayon `par_iter` scan from being aborted
    /// by a single pathologically long token.
    #[test]
    fn oversized_identifier_returns_none() {
        let huge = "x".repeat(65536);
        let rel = RelPath::from("a.rs");

        assert!(
            import_by_module(&huge, &rel, 0).is_none(),
            "import_by_module must return None for a 65536-byte module"
        );
        assert!(
            import_by_path(&rel, &huge, 0).is_none(),
            "import_by_path must return None for a 65536-byte module"
        );
        assert!(
            impl_by_trait(&huge, "T", &rel, 0).is_none(),
            "impl_by_trait must return None for a 65536-byte trait name"
        );
        assert!(
            impl_by_path(&rel, &huge, "T", 0).is_none(),
            "impl_by_path must return None for a 65536-byte trait name"
        );
    }
}
