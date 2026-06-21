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
///
/// Returns `None` when `bytes` exceeds 65535 bytes (the u16 ceiling). Path encoders that
/// call this for `RelPath` components may ignore the return value with `let _ = …` — real
/// file paths never hit 64 KiB. Identifier encoders (`symbol_by_name`, `call_by_callee`,
/// `import_by_module`, `import_by_path`, `impl_by_trait`, `impl_by_path`) return `Option`
/// and propagate `None` to their callers so that pathologically long tokens are silently
/// skipped rather than panicking inside a rayon `par_iter`.
fn write_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Option<()> {
    let len = u16::try_from(bytes.len()).ok()?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Some(())
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
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
    out.extend_from_slice(&start_byte.to_be_bytes());
    out
}

/// Prefix bytes for "all symbols in this file" — feed to `keyspace.prefix(..)`.
pub fn symbols_by_path_prefix(rel: &RelPath) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len());
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
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
///
/// Returns `None` when `name` exceeds 65535 bytes. The caller skips the secondary-index
/// entry but still writes the primary `symbols_by_path` entry so the outline stays complete.
pub fn symbol_by_name(
    name: &str,
    kind: SymbolKind,
    rel: &RelPath,
    start_byte: u32,
) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(2 + name.len() + 1 + 2 + rel.as_bytes().len() + 4);
    write_len_prefixed(&mut out, name.as_bytes())?;
    out.push(symbol_kind_byte(kind));
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
    out.extend_from_slice(&start_byte.to_be_bytes());
    Some(out)
}

pub fn symbols_by_name_prefix(name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + name.len());
    let _ = write_len_prefixed(&mut out, name.as_bytes());
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
///
/// Returns `None` when `callee` exceeds 65535 bytes. The caller skips the secondary-index
/// entry but still writes the primary `calls_by_path` entry so the call record stays complete.
pub fn call_by_callee(callee: &str, rel: &RelPath, start_byte: u32) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(2 + callee.len() + 2 + rel.as_bytes().len() + 4);
    write_len_prefixed(&mut out, callee.as_bytes())?;
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
    out.extend_from_slice(&start_byte.to_be_bytes());
    Some(out)
}

pub fn calls_by_callee_prefix(callee: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + callee.len());
    let _ = write_len_prefixed(&mut out, callee.as_bytes());
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
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
    out.extend_from_slice(&start_byte.to_be_bytes());
    out
}

pub fn calls_by_path_prefix(rel: &RelPath) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len());
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
    out
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
pub fn impl_by_trait(
    trait_name: &str,
    impl_type: &str,
    rel: &RelPath,
    start_byte: u32,
) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(
        2 + trait_name.len() + 2 + impl_type.len() + 2 + rel.as_bytes().len() + 4,
    );
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
pub fn impl_by_path(
    rel: &RelPath,
    trait_name: &str,
    impl_type: &str,
    start_byte: u32,
) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(
        2 + rel.as_bytes().len() + 2 + trait_name.len() + 2 + impl_type.len() + 4,
    );
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

// ─── memory_by_key ───────────────────────────────────────────────────────────

/// Visibility ordinal for the **group** (shared) memory tier. Stable, append-only.
pub const MEMORY_VIS_GROUP: u8 = 0;
/// Visibility ordinal for the **individual** (per-agent) memory tier. Stable, append-only.
pub const MEMORY_VIS_INDIVIDUAL: u8 = 1;

/// `memory_by_key`:
/// `u16:scope_len ‖ scope ‖ NUL ‖ vis_byte ‖ u16:owner_len ‖ owner ‖ NUL ‖ u16:key_len ‖ key`.
///
/// The `(scope, vis_byte, owner)` triple forms the namespace; placing it ahead of the key
/// keeps every namespace's keys contiguous, so a [`memory_by_key_ns_prefix`] range scan
/// returns exactly one namespace's entries. `vis_byte` is one of [`MEMORY_VIS_GROUP`] /
/// [`MEMORY_VIS_INDIVIDUAL`]. `owner` is the empty string for the group tier and the
/// validated `AgentId` for the individual tier (NUL-free by construction).
pub fn memory_by_key(scope: &str, vis_byte: u8, owner: &str, key: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + scope.len() + 1 + 1 + 2 + owner.len() + 1 + 2 + key.len());
    let _ = write_len_prefixed(&mut out, scope.as_bytes());
    out.push(0u8);
    out.push(vis_byte);
    let _ = write_len_prefixed(&mut out, owner.as_bytes());
    out.push(0u8);
    let _ = write_len_prefixed(&mut out, key.as_bytes());
    out
}

/// Prefix bytes for "all memory entries in this `(scope, vis_byte, owner)` namespace" —
/// everything up to and including the owner's NUL separator. Feed to `keyspace.prefix(..)`
/// or use as the lower bound of a range scan.
pub fn memory_by_key_ns_prefix(scope: &str, vis_byte: u8, owner: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + scope.len() + 1 + 1 + 2 + owner.len() + 1);
    let _ = write_len_prefixed(&mut out, scope.as_bytes());
    out.push(0u8);
    out.push(vis_byte);
    let _ = write_len_prefixed(&mut out, owner.as_bytes());
    out.push(0u8);
    out
}

/// Prefix bytes for "every memory entry in this `scope`" — across all visibility tiers and
/// owners. Because `scope` is length-prefixed, this prefix bounds exactly one scope's keys
/// (a longer scope encodes a different `u16` length, so no spillover). Used by the background
/// rescan audit to scope its scan to one repo without enumerating per-agent owners.
pub fn memory_scope_prefix(scope: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + scope.len() + 1);
    let _ = write_len_prefixed(&mut out, scope.as_bytes());
    out.push(0u8);
    out
}

/// Decode `(scope, vis_byte, owner, key)` from a raw `memory_by_key` key buffer.
pub fn parse_memory_by_key(buf: &[u8]) -> Option<(String, u8, String, String)> {
    let mut c = 0;
    let scope = String::from_utf8(read_len_prefixed(buf, &mut c)?).ok()?;
    if buf.len() <= c {
        return None;
    }
    c += 1; // skip NUL separator after scope
    if buf.len() <= c {
        return None;
    }
    let vis_byte = buf[c];
    c += 1;
    let owner = String::from_utf8(read_len_prefixed(buf, &mut c)?).ok()?;
    if buf.len() <= c {
        return None;
    }
    c += 1; // skip NUL separator after owner
    let key = String::from_utf8(read_len_prefixed(buf, &mut c)?).ok()?;
    Some((scope, vis_byte, owner, key))
}

/// Zero-copy decode of just the trailing `key` from a raw `memory_by_key` buffer, skipping the
/// scope/vis_byte/owner namespace prefix without allocating. Use on scan paths (e.g.
/// `memory_list`) that only need the key and discard the namespace components.
pub fn parse_memory_key_only(buf: &[u8]) -> Option<&str> {
    let mut c = 0;
    read_len_prefixed_ref(buf, &mut c)?; // skip scope
    c += 1; // NUL after scope
    if buf.len() <= c {
        return None;
    }
    c += 1; // vis_byte
    read_len_prefixed_ref(buf, &mut c)?; // skip owner
    if buf.len() <= c {
        return None;
    }
    c += 1; // NUL after owner
    let key = read_len_prefixed_ref(buf, &mut c)?;
    std::str::from_utf8(key).ok()
}

// ─── proposals ─────────────────────────────────────────────────────────────
//
// The `proposals` keyspace holds propose-don't-commit governance candidates (W11). Archived
// stale memories (W10) live in a separate `memory_archive` keyspace but reuse the
// `memory_by_key` encoder above — archive rows are keyed identically to their live form.

/// Proposal kind ordinal for a **memory** candidate. Stable, append-only.
pub const PROPOSAL_KIND_MEMORY: u8 = 0;
/// Proposal kind ordinal for a **skill** candidate (co-change association-rule). Stable, append-only.
pub const PROPOSAL_KIND_SKILL: u8 = 1;
/// Tombstone kind — written when a proposal is rejected so re-mining cannot resurface it.
/// Value bytes are empty (marker only). Stable, append-only.
pub const PROPOSAL_KIND_TOMBSTONE: u8 = 2;

/// `proposal_by_id`: `u16:scope_len ‖ scope ‖ NUL ‖ kind_byte ‖ u16:id_len ‖ id`.
///
/// `(scope, kind_byte)` is the namespace; `id` is the content-addressed proposal id (hex blake3
/// of the normalized candidate) so re-mining the same candidate overwrites rather than dupes.
/// Layout mirrors [`memory_by_key`]: the namespace prefix sorts contiguously, so a
/// [`proposal_ns_prefix`] range scan returns exactly one `(scope, kind)` namespace.
pub fn proposal_by_id(scope: &str, kind_byte: u8, id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + scope.len() + 1 + 1 + 2 + id.len());
    let _ = write_len_prefixed(&mut out, scope.as_bytes());
    out.push(0u8);
    out.push(kind_byte);
    let _ = write_len_prefixed(&mut out, id.as_bytes());
    out
}

/// Prefix bytes for "all proposals in this `(scope, kind_byte)` namespace" — feed to a range scan.
pub fn proposal_ns_prefix(scope: &str, kind_byte: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + scope.len() + 1 + 1);
    let _ = write_len_prefixed(&mut out, scope.as_bytes());
    out.push(0u8);
    out.push(kind_byte);
    out
}

/// Decode `(scope, kind_byte, id)` from a raw `proposal_by_id` key buffer.
pub fn parse_proposal_by_id(buf: &[u8]) -> Option<(String, u8, String)> {
    let mut c = 0;
    let scope = String::from_utf8(read_len_prefixed(buf, &mut c)?).ok()?;
    if buf.len() <= c {
        return None;
    }
    c += 1; // skip NUL separator after scope
    if buf.len() <= c {
        return None;
    }
    let kind_byte = buf[c];
    c += 1;
    let id = String::from_utf8(read_len_prefixed(buf, &mut c)?).ok()?;
    Some((scope, kind_byte, id))
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
        let key = symbol_by_name("alpha", SymbolKind::Function, &rel, 42).unwrap();
        let (name, kind, back, start) = parse_symbol_by_name(&key).unwrap();
        assert_eq!(name, "alpha");
        assert_eq!(kind, SymbolKind::Function);
        assert_eq!(back, rel);
        assert_eq!(start, 42);
    }

    #[test]
    fn call_by_callee_roundtrips() {
        let rel = RelPath::from("src/main.rs");
        let key = call_by_callee("spawn", &rel, 999).unwrap();
        let (callee, back, start) = parse_call_by_callee(&key).unwrap();
        assert_eq!(callee, "spawn");
        assert_eq!(back, rel);
        assert_eq!(start, 999);
    }

    #[test]
    fn import_by_module_roundtrips() {
        let rel = RelPath::from("src/foo.py");
        let key = import_by_module("os.path", &rel, 0).unwrap();
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
        let key_foo = call_by_callee("Foo", &rel, 1).unwrap();
        let key_foobar = call_by_callee("Foobar", &rel, 1).unwrap();
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

    /// All six identifier-encoding functions must return `None` at the 65536-byte boundary
    /// rather than panicking. This protects the rayon `par_iter` scan from being aborted
    /// by a single pathologically long token.
    #[test]
    fn oversized_identifier_returns_none() {
        let huge = "x".repeat(65536);
        let rel = RelPath::from("a.rs");

        assert!(
            symbol_by_name(&huge, SymbolKind::Function, &rel, 0).is_none(),
            "symbol_by_name must return None for a 65536-byte name"
        );
        assert!(
            call_by_callee(&huge, &rel, 0).is_none(),
            "call_by_callee must return None for a 65536-byte callee"
        );
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

    #[test]
    fn memory_by_key_roundtrips_group() {
        let raw = memory_by_key("scope-a", MEMORY_VIS_GROUP, "", "my.key");
        assert_eq!(
            parse_memory_by_key(&raw),
            Some((
                "scope-a".to_string(),
                MEMORY_VIS_GROUP,
                String::new(),
                "my.key".to_string()
            ))
        );
    }

    #[test]
    fn memory_by_key_roundtrips_individual() {
        let raw = memory_by_key("scope-a", MEMORY_VIS_INDIVIDUAL, "agent-7", "my.key");
        assert_eq!(
            parse_memory_by_key(&raw),
            Some((
                "scope-a".to_string(),
                MEMORY_VIS_INDIVIDUAL,
                "agent-7".to_string(),
                "my.key".to_string()
            ))
        );
    }

    /// The zero-copy `parse_memory_key_only` must return exactly the `key` component that the
    /// allocating `parse_memory_by_key` yields, for every namespace shape — including keys whose
    /// own bytes contain the NUL-adjacent separators and length-prefix-sized values.
    #[test]
    fn parse_memory_key_only_matches_full_parse() {
        let cases = [
            ("scope-a", MEMORY_VIS_GROUP, "", "my.key"),
            ("scope-a", MEMORY_VIS_INDIVIDUAL, "agent-7", "ns:sub.key"),
            ("", MEMORY_VIS_GROUP, "", ""),
            ("s", MEMORY_VIS_INDIVIDUAL, "owner-with-dashes", "k"),
            (
                "scope/with/slashes",
                MEMORY_VIS_GROUP,
                "",
                "key.with.many.dots",
            ),
        ];
        for (scope, vis, owner, key) in cases {
            let raw = memory_by_key(scope, vis, owner, key);
            let full = parse_memory_by_key(&raw).map(|(_, _, _, k)| k);
            let only = parse_memory_key_only(&raw).map(str::to_string);
            assert_eq!(only, full, "key-only parse diverged for key {key:?}");
            assert_eq!(only.as_deref(), Some(key));
        }
    }

    #[test]
    fn parse_memory_key_only_rejects_truncated_buffer() {
        let raw = memory_by_key("scope-a", MEMORY_VIS_INDIVIDUAL, "agent-7", "my.key");
        // Lop off the trailing key bytes: the length prefix promises more than remains.
        assert_eq!(parse_memory_key_only(&raw[..raw.len() - 3]), None);
        assert_eq!(parse_memory_key_only(&[]), None);
    }

    /// A group key and an individual key for the same `(scope, key)` must live in
    /// disjoint namespaces: neither key may fall within the other's namespace prefix.
    #[test]
    fn memory_namespace_prefixes_do_not_overlap() {
        let scope = "scope-a";
        let key = "shared.key";
        let group_key = memory_by_key(scope, MEMORY_VIS_GROUP, "", key);
        let indiv_key = memory_by_key(scope, MEMORY_VIS_INDIVIDUAL, "agent-7", key);

        let group_prefix = memory_by_key_ns_prefix(scope, MEMORY_VIS_GROUP, "");
        let indiv_prefix = memory_by_key_ns_prefix(scope, MEMORY_VIS_INDIVIDUAL, "agent-7");

        assert!(
            group_key.starts_with(&group_prefix),
            "group key must extend the group namespace prefix"
        );
        assert!(
            indiv_key.starts_with(&indiv_prefix),
            "individual key must extend the individual namespace prefix"
        );
        assert!(
            !group_key.starts_with(&indiv_prefix),
            "a group key must NOT fall within an individual namespace prefix"
        );
        assert!(
            !indiv_key.starts_with(&group_prefix),
            "an individual key must NOT fall within the group namespace prefix"
        );
    }

    /// Two different agents' individual namespaces for the same scope+key are disjoint.
    #[test]
    fn memory_individual_namespaces_isolate_by_owner() {
        let scope = "scope-a";
        let key = "k";
        let a_key = memory_by_key(scope, MEMORY_VIS_INDIVIDUAL, "agent-a", key);
        let b_prefix = memory_by_key_ns_prefix(scope, MEMORY_VIS_INDIVIDUAL, "agent-b");
        assert!(
            !a_key.starts_with(&b_prefix),
            "agent-a's key must NOT fall within agent-b's namespace prefix"
        );
    }
}
