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
//! `(rel_path, byte offset)` position in the scanned tree. The governance-tier keyspaces
//! (agent memory, mined proposals), which are addressed by a `(scope, ordinal, owner)`
//! namespace instead, live in [`crate::index::keys_governance`] and are re-exported below so
//! the `crate::index::keys::*` paths stay stable.

use crate::extract::SymbolKind;
use crate::path::RelPath;

pub use crate::index::keys_governance::{
    MEMORY_VIS_GROUP, MEMORY_VIS_INDIVIDUAL, PROPOSAL_KIND_MEMORY, PROPOSAL_KIND_SKILL, PROPOSAL_KIND_TOMBSTONE,
    memory_by_key, memory_by_key_ns_prefix, memory_scope_prefix, parse_memory_by_key, parse_memory_key_only,
    parse_proposal_by_id, proposal_by_id, proposal_ns_prefix,
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
pub fn symbol_by_name(name: &str, kind: SymbolKind, rel: &RelPath, start_byte: u32) -> Option<Vec<u8>> {
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

/// `code_bm25_postings`: `u16:len(term) ‖ term ‖ u16:len(chunk_id) ‖ chunk_id`.
///
/// Returns `None` when `term` exceeds 65535 bytes (the BM25 tokenizer caps term length far below
/// this, so `None` is unreachable in practice — the guard mirrors the other identifier encoders so
/// a pathological token is skipped rather than panicking inside a rayon `par_iter`).
pub fn code_bm25_posting(term: &str, chunk_id: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(2 + term.len() + 2 + chunk_id.len());
    write_len_prefixed(&mut out, term.as_bytes())?;
    let _ = write_len_prefixed(&mut out, chunk_id.as_bytes());
    Some(out)
}

/// Prefix bytes for "every chunk containing `term`" — feed to `keyspace.prefix(..)`.
pub fn code_bm25_postings_prefix(term: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + term.len());
    let _ = write_len_prefixed(&mut out, term.as_bytes());
    out
}

/// Decode the trailing `chunk_id` from a `code_bm25_postings` key, skipping the (known-from-prefix)
/// term without allocating it. Returns a borrowed slice into `key`.
pub fn parse_code_bm25_posting_chunk_id(key: &[u8]) -> Option<&str> {
    let mut c = 0;
    read_len_prefixed_ref(key, &mut c)?;
    let chunk_id = read_len_prefixed_ref(key, &mut c)?;
    std::str::from_utf8(chunk_id).ok()
}

/// Decode `(term, chunk_id)` from a raw `code_bm25_postings` key. The allocating companion to
/// [`parse_code_bm25_posting_chunk_id`]; used by the roundtrip tests.
pub fn parse_code_bm25_posting(key: &[u8]) -> Option<(String, String)> {
    let mut c = 0;
    let term = String::from_utf8(read_len_prefixed(key, &mut c)?).ok()?;
    let chunk_id = String::from_utf8(read_len_prefixed(key, &mut c)?).ok()?;
    Some((term, chunk_id))
}

/// Encode a `code_bm25_postings` value: `tf:u32_be ‖ doclen:u32_be`. Both inlined so a single
/// term-prefix scan carries the term frequency and the document length the scorer needs.
pub fn code_bm25_posting_value(tf: u32, doclen: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&tf.to_be_bytes());
    out[4..].copy_from_slice(&doclen.to_be_bytes());
    out
}

/// Decode `(tf, doclen)` from a `code_bm25_postings` value.
pub fn parse_code_bm25_posting_value(value: &[u8]) -> Option<(u32, u32)> {
    if value.len() < 8 {
        return None;
    }
    let tf = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
    let doclen = u32::from_be_bytes([value[4], value[5], value[6], value[7]]);
    Some((tf, doclen))
}

/// `code_bm25_by_path`: `u16:len(rel) ‖ rel ‖ u16:len(chunk_id) ‖ chunk_id`. The forward map keyed
/// by file so re-scan deletion is O(prefix). Both components are path/hash-shaped and never reach
/// the 64 KiB ceiling.
pub fn code_bm25_by_path(rel: &RelPath, chunk_id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len() + 2 + chunk_id.len());
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
    let _ = write_len_prefixed(&mut out, chunk_id.as_bytes());
    out
}

/// Prefix bytes for "every BM25 chunk entry in this file" — used for the per-file delete on re-scan.
pub fn code_bm25_by_path_prefix(rel: &RelPath) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + rel.as_bytes().len());
    let _ = write_len_prefixed(&mut out, rel.as_bytes());
    out
}

pub fn parse_code_bm25_by_path(key: &[u8]) -> Option<(RelPath, String)> {
    let mut c = 0;
    let rel = read_len_prefixed_ref(key, &mut c)?;
    let chunk_id = String::from_utf8(read_len_prefixed(key, &mut c)?).ok()?;
    Some((RelPath::from(rel), chunk_id))
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
        SymbolKind::Field => 16,
        SymbolKind::Variable => 17,
        SymbolKind::EnumVariant => 18,
        SymbolKind::Constructor => 19,
        SymbolKind::Decorator => 20,
        SymbolKind::Heading => 21,
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
        21 => SymbolKind::Heading,
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
        assert!(key_foo.starts_with(&prefix_foo), "Foo's key must extend the Foo prefix");
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
    fn code_bm25_posting_roundtrips() {
        let key = code_bm25_posting("spawn", "abcd1234:3").unwrap();
        let (term, chunk_id) = parse_code_bm25_posting(&key).unwrap();
        assert_eq!(term, "spawn");
        assert_eq!(chunk_id, "abcd1234:3");
        assert_eq!(parse_code_bm25_posting_chunk_id(&key), Some("abcd1234:3"));
    }

    #[test]
    fn code_bm25_posting_value_roundtrips() {
        let value = code_bm25_posting_value(7, 142);
        assert_eq!(parse_code_bm25_posting_value(&value), Some((7, 142)));
        assert_eq!(parse_code_bm25_posting_value(&value[..7]), None);
    }

    /// A `spawn` posting prefix must not bleed into `spawn_blocking` — the whole point of
    /// length-prefixing the term component.
    #[test]
    fn code_bm25_posting_prefix_isolates_terms() {
        let key_spawn = code_bm25_posting("spawn", "h:1").unwrap();
        let key_spawn_blocking = code_bm25_posting("spawnblocking", "h:1").unwrap();
        let prefix = code_bm25_postings_prefix("spawn");
        assert!(
            key_spawn.starts_with(&prefix),
            "spawn's key must extend the spawn prefix"
        );
        assert!(
            !key_spawn_blocking.starts_with(&prefix),
            "spawnblocking's key must NOT match the spawn prefix"
        );
    }

    #[test]
    fn code_bm25_by_path_roundtrips_and_isolates() {
        let rel_a = RelPath::from("src/foo.rs");
        let rel_b = RelPath::from("src/foo.rs.bak");
        let key = code_bm25_by_path(&rel_a, "hash:0");
        let (back, chunk_id) = parse_code_bm25_by_path(&key).unwrap();
        assert_eq!(back, rel_a);
        assert_eq!(chunk_id, "hash:0");

        let key_b = code_bm25_by_path(&rel_b, "hash:0");
        let prefix_a = code_bm25_by_path_prefix(&rel_a);
        assert!(key.starts_with(&prefix_a), "rel_a's key must extend rel_a's prefix");
        assert!(
            !key_b.starts_with(&prefix_a),
            "rel_b's key must NOT match rel_a's prefix"
        );
    }
}
