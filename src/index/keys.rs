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

use crate::extract::SymbolKind;
use crate::path::RelPath;

/// `u16:name_len ‖ name`. Internal helper.
fn write_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u16::try_from(bytes.len()).expect("identifier > 64 KiB — pathological input");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn read_len_prefixed(buf: &[u8], cursor: &mut usize) -> Option<Vec<u8>> {
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
fn read_len_prefixed_ref<'buf>(buf: &'buf [u8], cursor: &mut usize) -> Option<&'buf [u8]> {
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

/// `symbols_by_path`: `u16:len(rel) ‖ rel ‖ start_byte:u32_be`.
pub fn symbol_by_path(rel: &RelPath, start_byte: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len() + 4);
    write_len_prefixed(&mut out, rel.as_bytes());
    out.extend_from_slice(&start_byte.to_be_bytes());
    out
}

/// Prefix bytes for "all symbols in this file" — feed to `keyspace.prefix(..)`.
pub fn symbols_by_path_prefix(rel: &RelPath) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len());
    write_len_prefixed(&mut out, rel.as_bytes());
    out
}

pub fn parse_symbol_by_path(key: &[u8]) -> Option<(RelPath, u32)> {
    let mut c = 0;
    let rel = read_len_prefixed_ref(key, &mut c)?;
    if key.len() < c + 4 {
        return None;
    }
    let start = u32::from_be_bytes([key[c], key[c + 1], key[c + 2], key[c + 3]]);
    Some((RelPath::from(rel), start))
}

/// `symbols_by_name`: `u16:len(name) ‖ name ‖ kind:u8 ‖ u16:len(rel) ‖ rel ‖ start_byte:u32_be`.
pub fn symbol_by_name(name: &str, kind: SymbolKind, rel: &RelPath, start_byte: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + name.len() + 1 + 2 + rel.as_bytes().len() + 4);
    write_len_prefixed(&mut out, name.as_bytes());
    out.push(symbol_kind_byte(kind));
    write_len_prefixed(&mut out, rel.as_bytes());
    out.extend_from_slice(&start_byte.to_be_bytes());
    out
}

pub fn symbols_by_name_prefix(name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + name.len());
    write_len_prefixed(&mut out, name.as_bytes());
    out
}

pub fn parse_symbol_by_name(key: &[u8]) -> Option<(String, SymbolKind, RelPath, u32)> {
    let mut c = 0;
    let name_bytes = read_len_prefixed(key, &mut c)?;
    let name = String::from_utf8(name_bytes).ok()?;
    if key.len() < c + 1 {
        return None;
    }
    let kind = symbol_kind_from_byte(key[c]);
    c += 1;
    let rel = read_len_prefixed_ref(key, &mut c)?;
    if key.len() < c + 4 {
        return None;
    }
    let start = u32::from_be_bytes([key[c], key[c + 1], key[c + 2], key[c + 3]]);
    Some((name, kind, RelPath::from(rel), start))
}

/// `calls_by_callee`: `u16:len(callee) ‖ callee ‖ u16:len(rel) ‖ rel ‖ start_byte:u32_be`.
pub fn call_by_callee(callee: &str, rel: &RelPath, start_byte: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + callee.len() + 2 + rel.as_bytes().len() + 4);
    write_len_prefixed(&mut out, callee.as_bytes());
    write_len_prefixed(&mut out, rel.as_bytes());
    out.extend_from_slice(&start_byte.to_be_bytes());
    out
}

pub fn calls_by_callee_prefix(callee: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + callee.len());
    write_len_prefixed(&mut out, callee.as_bytes());
    out
}

pub fn parse_call_by_callee(key: &[u8]) -> Option<(String, RelPath, u32)> {
    let mut c = 0;
    let callee = String::from_utf8(read_len_prefixed(key, &mut c)?).ok()?;
    let rel = read_len_prefixed_ref(key, &mut c)?;
    if key.len() < c + 4 {
        return None;
    }
    let start = u32::from_be_bytes([key[c], key[c + 1], key[c + 2], key[c + 3]]);
    Some((callee, RelPath::from(rel), start))
}

/// `calls_by_path`: same shape as `symbols_by_path` so iterating "all calls in this file"
/// works the same way.
pub fn call_by_path(rel: &RelPath, start_byte: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len() + 4);
    write_len_prefixed(&mut out, rel.as_bytes());
    out.extend_from_slice(&start_byte.to_be_bytes());
    out
}

pub fn calls_by_path_prefix(rel: &RelPath) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len());
    write_len_prefixed(&mut out, rel.as_bytes());
    out
}

/// `imports_by_module`: `u16:len(module) ‖ module ‖ u16:len(rel) ‖ rel ‖ start_byte:u32_be`.
pub fn import_by_module(module: &str, rel: &RelPath, start_byte: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + module.len() + 2 + rel.as_bytes().len() + 4);
    write_len_prefixed(&mut out, module.as_bytes());
    write_len_prefixed(&mut out, rel.as_bytes());
    out.extend_from_slice(&start_byte.to_be_bytes());
    out
}

pub fn imports_by_module_prefix(module: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + module.len());
    write_len_prefixed(&mut out, module.as_bytes());
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
pub fn import_by_path(rel: &RelPath, module: &str, start_byte: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len() + 2 + module.len() + 4);
    write_len_prefixed(&mut out, rel.as_bytes());
    write_len_prefixed(&mut out, module.as_bytes());
    out.extend_from_slice(&start_byte.to_be_bytes());
    out
}

pub fn imports_by_path_prefix(rel: &RelPath) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len());
    write_len_prefixed(&mut out, rel.as_bytes());
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

// ─── memory_by_key ───────────────────────────────────────────────────────────

/// `memory_by_key`: `u16:scope_len ‖ scope ‖ NUL ‖ u16:key_len ‖ key`.
pub fn memory_by_key(scope: &str, key: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + scope.len() + 1 + 2 + key.len());
    write_len_prefixed(&mut out, scope.as_bytes());
    out.push(0u8);
    write_len_prefixed(&mut out, key.as_bytes());
    out
}

/// Prefix bytes for "all memory entries in this scope" — feed to `keyspace.prefix(..)`.
pub fn memory_by_key_scope_prefix(scope: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + scope.len() + 1);
    write_len_prefixed(&mut out, scope.as_bytes());
    out.push(0u8);
    out
}

/// Decode `scope` and `key` from a raw `memory_by_key` key buffer.
pub fn parse_memory_by_key(buf: &[u8]) -> Option<(String, String)> {
    let mut c = 0;
    let scope = String::from_utf8(read_len_prefixed(buf, &mut c)?).ok()?;
    if buf.len() <= c {
        return None;
    }
    c += 1; // skip NUL separator
    let key = String::from_utf8(read_len_prefixed(buf, &mut c)?).ok()?;
    Some((scope, key))
}

/// One-byte ordinal for a `SymbolKind`. Stable across releases so existing keys stay valid;
/// new variants extend the tail. Keep the explicit assignments — accidentally reordering
/// would silently miscategorize cached entries.
fn symbol_kind_byte(k: SymbolKind) -> u8 {
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
        // Append-only past this line — see `index-keyspace-evolution` skill.
        SymbolKind::Field => 16,
        SymbolKind::Variable => 17,
        SymbolKind::EnumVariant => 18,
        SymbolKind::Constructor => 19,
        SymbolKind::Decorator => 20,
    }
}

fn symbol_kind_from_byte(b: u8) -> SymbolKind {
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
        _ => SymbolKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_by_path_roundtrips() {
        let rel = RelPath::from("src/lib.rs");
        let key = symbol_by_path(&rel, 1234);
        let (back, start) = parse_symbol_by_path(&key).unwrap();
        assert_eq!(back, rel);
        assert_eq!(start, 1234);
    }

    #[test]
    fn symbol_by_name_roundtrips_with_kind() {
        let rel = RelPath::from("src/foo.rs");
        let key = symbol_by_name("alpha", SymbolKind::Function, &rel, 42);
        let (name, kind, back, start) = parse_symbol_by_name(&key).unwrap();
        assert_eq!(name, "alpha");
        assert_eq!(kind, SymbolKind::Function);
        assert_eq!(back, rel);
        assert_eq!(start, 42);
    }

    #[test]
    fn call_by_callee_roundtrips() {
        let rel = RelPath::from("src/main.rs");
        let key = call_by_callee("spawn", &rel, 999);
        let (callee, back, start) = parse_call_by_callee(&key).unwrap();
        assert_eq!(callee, "spawn");
        assert_eq!(back, rel);
        assert_eq!(start, 999);
    }

    #[test]
    fn import_by_module_roundtrips() {
        let rel = RelPath::from("src/foo.py");
        let key = import_by_module("os.path", &rel, 0);
        let (module, back, start) = parse_import_by_module(&key).unwrap();
        assert_eq!(module, "os.path");
        assert_eq!(back, rel);
        assert_eq!(start, 0);
    }

    /// The whole point of length-prefixing: `Foo` and `Foobar` must never collide on
    /// a prefix scan of `Foo`. Without length-prefixing, the simple `\0` separator would
    /// fail for callee names containing embedded `\0` bytes (rare but possible).
    #[test]
    fn prefix_scan_isolates_callees() {
        let rel = RelPath::from("a.rs");
        let key_foo = call_by_callee("Foo", &rel, 1);
        let key_foobar = call_by_callee("Foobar", &rel, 1);
        let prefix_foo = calls_by_callee_prefix("Foo");
        assert!(
            key_foo.starts_with(&prefix_foo),
            "Foo's key must extend the Foo prefix"
        );
        assert!(
            !key_foobar.starts_with(&prefix_foo),
            "Foobar's key must NOT match the Foo prefix"
        );
    }

    #[test]
    fn import_by_path_roundtrips() {
        let rel = RelPath::from("src/foo.py");
        let key = import_by_path(&rel, "os.path", 42);
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
        let key_a = import_by_path(&rel_a, "os", 0);
        let key_b = import_by_path(&rel_b, "os", 0);
        let prefix_a = imports_by_path_prefix(&rel_a);
        assert!(
            key_a.starts_with(&prefix_a),
            "rel_a's key must extend rel_a's prefix"
        );
        assert!(
            !key_b.starts_with(&prefix_a),
            "rel_b's key must NOT match rel_a's prefix"
        );
    }

    #[test]
    fn non_utf8_path_keys_roundtrip() {
        let rel = RelPath::from(b"f\xffoo.rs".as_slice());
        let key = symbol_by_path(&rel, 7);
        let (back, _) = parse_symbol_by_path(&key).unwrap();
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
}
