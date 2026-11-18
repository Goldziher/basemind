//! Keys of the symbol and call rows (ADR-0028 rule 2).
//!
//! A file's symbols and call sites follow from its content, language and extract epoch alone, so
//! every view of a repository group shares them: they are keyed by a 16-byte [`ContentKey`] of
//! those three, never by path. A view reads them through its live keys
//! ([`super::live_keys::LiveKeys`]), which map each content key to the view's paths holding it.
//!
//! Two shapes of key exist for the name-led keyspaces:
//!
//! - the stored ROW key: `u16:len(name) ‖ name ‖ [kind:u8] ‖ key:16 ‖ start:u32_be`;
//! - the HIT key a view hands its readers: the row key followed by the path of one live holder.
//!
//! Hit keys of one row differ only in their path tail, and every row key of a name has the same
//! length, so hit keys sort by name, content key, offset, then path (ADR-0028 rule 7), and a hit key
//! is a cursor that resumes inside its row.

use super::keys::{read_len_prefixed_ref, symbol_kind_byte, symbol_kind_from_byte, write_len_prefixed};
use crate::extract::SymbolKind;
use crate::path::RelPath;

/// Bytes of a [`ContentKey`].
pub const CONTENT_KEY_LEN: usize = 16;

/// The key a file's symbol and call rows are written under. See the module docs.
pub type ContentKey = [u8; CONTENT_KEY_LEN];

/// Bytes after the content key in every row key: the start offset.
const START_LEN: usize = 4;

/// The [`ContentKey`] of a file: 16 bytes of blake3 over the content hash, the language and the
/// extract epoch the file map records for it, each length-prefixed.
pub fn content_key(hash_hex: &str, language: &str, extract_epoch: u16) -> ContentKey {
    let mut input = Vec::with_capacity(40 + hash_hex.len() + language.len());
    input.extend_from_slice(b"basemind/content_key/1\0");
    input.extend_from_slice(&extract_epoch.to_be_bytes());
    for part in [hash_hex, language] {
        input.extend_from_slice(&u32::try_from(part.len()).unwrap_or(u32::MAX).to_be_bytes());
        input.extend_from_slice(part.as_bytes());
    }
    let digest = crate::hashing::hash_bytes(&input);
    let mut key = [0u8; CONTENT_KEY_LEN];
    key.copy_from_slice(&digest[..CONTENT_KEY_LEN]);
    key
}

/// `symbols_by_path` and `calls_by_path`: `key:16 ‖ start:u32_be`. The value is the msgpack
/// `Symbol` or `Call`.
pub fn content_row(content: &ContentKey, start_byte: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(CONTENT_KEY_LEN + START_LEN);
    out.extend_from_slice(content);
    out.extend_from_slice(&start_byte.to_be_bytes());
    out
}

/// Decode `(key, start)` from a `symbols_by_path` or `calls_by_path` key.
pub fn parse_content_row(key: &[u8]) -> Option<(ContentKey, u32)> {
    if key.len() != CONTENT_KEY_LEN + START_LEN {
        return None;
    }
    let content: ContentKey = key[..CONTENT_KEY_LEN].try_into().ok()?;
    let start = u32::from_be_bytes(key[CONTENT_KEY_LEN..].try_into().ok()?);
    Some((content, start))
}

/// `symbols_by_name` row: `u16:len(name) ‖ name ‖ kind:u8 ‖ key:16 ‖ start:u32_be`.
///
/// Returns `None` when `name` exceeds 65535 bytes. The caller skips the name row but still writes
/// the `symbols_by_path` row, so the content's symbols stay complete.
pub fn symbol_by_name(name: &str, kind: SymbolKind, content: &ContentKey, start_byte: u32) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(2 + name.len() + 1 + CONTENT_KEY_LEN + START_LEN);
    write_len_prefixed(&mut out, name.as_bytes())?;
    out.push(symbol_kind_byte(kind));
    out.extend_from_slice(content);
    out.extend_from_slice(&start_byte.to_be_bytes());
    Some(out)
}

/// Prefix bytes for "every symbol named `name`".
pub fn symbols_by_name_prefix(name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + name.len());
    let _ = write_len_prefixed(&mut out, name.as_bytes());
    out
}

/// The hit key of `symbols_by_name` for one holder: the row key followed by `rel`.
pub fn symbol_by_name_hit(
    name: &str,
    kind: SymbolKind,
    content: &ContentKey,
    start_byte: u32,
    rel: &RelPath,
) -> Option<Vec<u8>> {
    let mut out = symbol_by_name(name, kind, content, start_byte)?;
    out.extend_from_slice(rel.as_bytes());
    Some(out)
}

/// Decode `(name, kind, path, start)` from a `symbols_by_name` HIT key. A row key, which names no
/// path, does not decode.
pub fn parse_symbol_by_name(key: &[u8]) -> Option<(String, SymbolKind, RelPath, u32)> {
    let (row, path) = split_hit(key, NameRow::Symbol)?;
    if path.is_empty() {
        return None;
    }
    let mut c = 0;
    let name = std::str::from_utf8(read_len_prefixed_ref(row, &mut c)?)
        .ok()?
        .to_owned();
    let kind = symbol_kind_from_byte(row[c]);
    let start = u32::from_be_bytes(row[row.len() - START_LEN..].try_into().ok()?);
    Some((name, kind, RelPath::from(path), start))
}

/// `calls_by_callee` row: `u16:len(callee) ‖ callee ‖ key:16 ‖ start:u32_be`.
///
/// Returns `None` when `callee` exceeds 65535 bytes. The caller skips the name row but still writes
/// the `calls_by_path` row, so the content's call sites stay complete.
pub fn call_by_callee(callee: &str, content: &ContentKey, start_byte: u32) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(2 + callee.len() + CONTENT_KEY_LEN + START_LEN);
    write_len_prefixed(&mut out, callee.as_bytes())?;
    out.extend_from_slice(content);
    out.extend_from_slice(&start_byte.to_be_bytes());
    Some(out)
}

/// Prefix bytes for "every call of `callee`".
pub fn calls_by_callee_prefix(callee: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + callee.len());
    let _ = write_len_prefixed(&mut out, callee.as_bytes());
    out
}

/// The hit key of `calls_by_callee` for one holder: the row key followed by `rel`. The key a view
/// yields for the call site, and the cursor that resumes after it.
pub fn call_by_callee_hit(callee: &str, content: &ContentKey, start_byte: u32, rel: &RelPath) -> Option<Vec<u8>> {
    let mut out = call_by_callee(callee, content, start_byte)?;
    out.extend_from_slice(rel.as_bytes());
    Some(out)
}

/// Decode `(callee, path, start)` from a `calls_by_callee` HIT key. A row key, which names no path,
/// does not decode.
pub fn parse_call_by_callee(key: &[u8]) -> Option<(String, RelPath, u32)> {
    let (row, path) = split_hit(key, NameRow::Call)?;
    if path.is_empty() {
        return None;
    }
    let mut c = 0;
    let callee = std::str::from_utf8(read_len_prefixed_ref(row, &mut c)?)
        .ok()?
        .to_owned();
    let start = u32::from_be_bytes(row[row.len() - START_LEN..].try_into().ok()?);
    Some((callee, RelPath::from(path), start))
}

/// The content key of a `calls_by_callee` key, row or hit.
pub fn call_by_callee_content(key: &[u8]) -> Option<ContentKey> {
    NameRow::Call.content_of(split_hit(key, NameRow::Call)?.0)
}

/// Which name-led keyspace a key belongs to: the shapes differ by the kind byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameRow {
    /// `symbols_by_name`.
    Symbol,
    /// `calls_by_callee`.
    Call,
}

impl NameRow {
    /// Bytes between the name and the content key.
    const fn extra(self) -> usize {
        match self {
            NameRow::Symbol => 1,
            NameRow::Call => 0,
        }
    }

    /// The content key of a row key of this shape.
    pub fn content_of(self, row: &[u8]) -> Option<ContentKey> {
        let end = row.len().checked_sub(START_LEN)?;
        row.get(end.checked_sub(CONTENT_KEY_LEN)?..end)?.try_into().ok()
    }
}

/// Split a row or hit key of `shape` into its row key and its path tail (empty for a row key).
pub fn split_hit(key: &[u8], shape: NameRow) -> Option<(&[u8], &[u8])> {
    let name_len = usize::from(u16::from_be_bytes([*key.first()?, *key.get(1)?]));
    let row_len = 2 + name_len + shape.extra() + CONTENT_KEY_LEN + START_LEN;
    if key.len() < row_len {
        return None;
    }
    Some(key.split_at(row_len))
}

/// Serde for an optional [`ContentKey`] in the file map: a msgpack `bin` of 16 bytes, not an array
/// of 16 integers. Reads either form; a value of another length is an error.
pub mod opt_content_key {
    use serde::de::{self, Deserializer, SeqAccess, Visitor};
    use serde::ser::Serializer;

    use super::{CONTENT_KEY_LEN, ContentKey};

    struct Bytes<'a>(&'a [u8]);

    impl serde::Serialize for Bytes<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_bytes(self.0)
        }
    }

    /// Serialize `key` as bytes, or as none.
    pub fn serialize<S: Serializer>(key: &Option<ContentKey>, serializer: S) -> Result<S::Ok, S::Error> {
        match key {
            Some(key) => serializer.serialize_some(&Bytes(key)),
            None => serializer.serialize_none(),
        }
    }

    struct KeyVisitor;

    impl<'de> Visitor<'de> for KeyVisitor {
        type Value = ContentKey;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "{CONTENT_KEY_LEN} bytes")
        }

        fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<ContentKey, E> {
            bytes.try_into().map_err(|_| E::invalid_length(bytes.len(), &self))
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<ContentKey, A::Error> {
            let mut key = [0u8; CONTENT_KEY_LEN];
            for (at, byte) in key.iter_mut().enumerate() {
                *byte = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(at, &self))?;
            }
            if seq.next_element::<u8>()?.is_some() {
                return Err(de::Error::invalid_length(CONTENT_KEY_LEN + 1, &self));
            }
            Ok(key)
        }
    }

    struct Key(ContentKey);

    impl<'de> serde::Deserialize<'de> for Key {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_bytes(KeyVisitor).map(Key)
        }
    }

    /// Read a key written by [`serialize`].
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<ContentKey>, D::Error> {
        Ok(<Option<Key> as serde::Deserialize>::deserialize(deserializer)?.map(|key| key.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Entry {
        #[serde(default, with = "opt_content_key", skip_serializing_if = "Option::is_none")]
        key: Option<ContentKey>,
    }

    #[test]
    fn an_optional_content_key_is_written_as_sixteen_bytes_and_read_back() {
        let some = Entry {
            key: Some(content_key("h", "rust", 1)),
        };
        let bytes = rmp_serde::to_vec_named(&some).unwrap();
        assert_eq!(
            bytes.len(),
            1 + 4 + 2 + CONTENT_KEY_LEN,
            "map, key name, bin8 header, the key"
        );
        assert_eq!(rmp_serde::from_slice::<Entry>(&bytes).unwrap(), some);
        let none = Entry { key: None };
        let bytes = rmp_serde::to_vec_named(&none).unwrap();
        assert_eq!(
            rmp_serde::from_slice::<Entry>(&bytes).unwrap(),
            none,
            "absent reads as none"
        );
        let short = rmp_serde::to_vec_named(&serde_json::json!({ "key": serde_bytes_of(&[1, 2]) })).unwrap();
        assert!(
            rmp_serde::from_slice::<Entry>(&short).is_err(),
            "a key of another length is refused"
        );
    }

    /// A JSON value msgpack writes as an array of integers.
    fn serde_bytes_of(bytes: &[u8]) -> serde_json::Value {
        serde_json::Value::Array(bytes.iter().map(|byte| (*byte).into()).collect())
    }

    #[test]
    fn content_rows_roundtrip() {
        let content = content_key("abcd", "rust", 3);
        assert_eq!(parse_content_row(&content_row(&content, 1234)), Some((content, 1234)));
        assert_eq!(parse_content_row(&content_row(&content, 1)[..19]), None);
    }

    #[test]
    fn the_content_key_follows_hash_language_and_epoch() {
        let base = content_key("hash", "rust", 1);
        assert_eq!(base, content_key("hash", "rust", 1));
        assert_ne!(base, content_key("hash2", "rust", 1));
        assert_ne!(base, content_key("hash", "python", 1));
        assert_ne!(base, content_key("hash", "rust", 2));
        assert_ne!(
            content_key("ab", "c", 1),
            content_key("a", "bc", 1),
            "parts are length-prefixed"
        );
    }

    #[test]
    fn symbol_hits_roundtrip_and_rows_do_not_decode() {
        let content = content_key("h", "rust", 1);
        let rel = RelPath::from("src/foo.rs");
        let row = symbol_by_name("alpha", SymbolKind::Function, &content, 42).unwrap();
        let hit = symbol_by_name_hit("alpha", SymbolKind::Function, &content, 42, &rel).unwrap();
        assert!(hit.starts_with(&row));
        assert_eq!(
            parse_symbol_by_name(&hit),
            Some(("alpha".to_string(), SymbolKind::Function, rel, 42))
        );
        assert_eq!(parse_symbol_by_name(&row), None, "a row names no holder");
        assert_eq!(NameRow::Symbol.content_of(&row), Some(content));
    }

    #[test]
    fn call_hits_roundtrip_and_order_by_key_offset_then_path() {
        let content = content_key("h", "rust", 1);
        let (a, b) = (RelPath::from("a.rs"), RelPath::from("b.rs"));
        let hit = call_by_callee_hit("spawn", &content, 999, &b).unwrap();
        assert_eq!(parse_call_by_callee(&hit), Some(("spawn".to_string(), b.clone(), 999)));
        assert_eq!(call_by_callee_content(&hit), Some(content));
        let mut hits = [
            call_by_callee_hit("spawn", &content, 2, &a).unwrap(),
            call_by_callee_hit("spawn", &content, 1, &b).unwrap(),
            call_by_callee_hit("spawn", &content, 1, &a).unwrap(),
        ];
        hits.sort();
        let order: Vec<_> = hits.iter().map(|hit| parse_call_by_callee(hit).unwrap()).collect();
        assert_eq!(
            order,
            vec![
                ("spawn".to_string(), a.clone(), 1),
                ("spawn".to_string(), b, 1),
                ("spawn".to_string(), a, 2)
            ]
        );
    }

    /// `Foo` and `Foobar` never collide on a prefix scan of `Foo`.
    #[test]
    fn prefix_scan_isolates_names() {
        let content = content_key("h", "rust", 1);
        let prefix = calls_by_callee_prefix("Foo");
        assert!(call_by_callee("Foo", &content, 1).unwrap().starts_with(&prefix));
        assert!(!call_by_callee("Foobar", &content, 1).unwrap().starts_with(&prefix));
        let prefix = symbols_by_name_prefix("Foo");
        assert!(
            !symbol_by_name("Foobar", SymbolKind::Function, &content, 1)
                .unwrap()
                .starts_with(&prefix)
        );
    }

    #[test]
    fn non_utf8_paths_roundtrip_in_hits() {
        let content = content_key("h", "rust", 1);
        let rel = RelPath::from(b"f\xffoo.rs".as_slice());
        let hit = call_by_callee_hit("f", &content, 7, &rel).unwrap();
        assert_eq!(parse_call_by_callee(&hit).unwrap().1.as_bytes(), rel.as_bytes());
    }

    #[test]
    fn oversized_names_return_none() {
        let huge = "x".repeat(65536);
        let content = content_key("h", "rust", 1);
        assert!(symbol_by_name(&huge, SymbolKind::Function, &content, 0).is_none());
        assert!(call_by_callee(&huge, &content, 0).is_none());
    }
}
