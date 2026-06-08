//! `RelPath` — repository-relative path that survives non-UTF-8 bytes end to end.
//!
//! basemind started life with `String` path fields throughout. That works for ~99% of OSS
//! repositories but silently drops files on filesystems that store non-UTF-8 path bytes
//! (Linux ext4 with deliberately exotic names, archives extracted with mixed encodings,
//! etc.). This module replaces `String` at the path boundary with a typed wrapper around
//! `BString` so the raw bytes flow through the scanner → store → MCP layer without a
//! lossy round-trip.
//!
//! ## Wire format
//!
//! Serde serializes a `RelPath` as a plain JSON / msgpack string **when the bytes are
//! valid UTF-8** (the overwhelmingly common case — clients see no change). When they
//! aren't, it falls back to a `{"bytes": [u8...]}` discriminator so the bytes round-trip
//! without ambiguity. The deserializer accepts both shapes, plus raw msgpack `bin` blobs
//! (which is how rmp-serde sometimes encodes byte sequences).
//!
//! ## Windows
//!
//! `OsStr::as_encoded_bytes` exposes Windows paths as *WTF-8* — a UTF-8 superset that can
//! losslessly round-trip ill-formed UTF-16 (unpaired surrogates). `RelPath` stores those
//! bytes as-is; `Display` interprets them as WTF-8 and renders unpaired surrogates as
//! `\u{NNNN}` escapes. To convert back to an `OsStr` for filesystem operations, use
//! `OsStr::from_encoded_bytes_unchecked` (stable since Rust 1.74). **Never** reach for
//! `String::from_utf8_lossy` on the buffer — it silently corrupts the WTF-8 stream.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};

use bstr::{BStr, BString, ByteSlice};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct RelPath(BString);

impl RelPath {
    pub fn new<B: Into<BString>>(bytes: B) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_slice()
    }

    pub fn as_bstr(&self) -> &BStr {
        self.0.as_bstr()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// True when the path encodes as valid UTF-8. The common case in practice; we treat
    /// it as a fast-path in serialization and rendering.
    pub fn is_utf8(&self) -> bool {
        std::str::from_utf8(self.0.as_slice()).is_ok()
    }

    /// Borrow the path as a `&str` when it's valid UTF-8. Returns `None` for paths with
    /// invalid byte sequences.
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(self.0.as_slice()).ok()
    }

    /// Lossy `&str` view — invalid UTF-8 sequences become U+FFFD. Use for error messages
    /// and tracing only; never for hashing or filesystem ops, since the substitution
    /// destroys the original bytes.
    pub fn to_str_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(self.0.as_slice())
    }

    /// Borrow the path as an `&OsStr`. Lossless on Unix (raw bytes). On Windows the bytes
    /// must be WTF-8 (the output of `OsStr::as_encoded_bytes`); construction from `&[u8]` /
    /// `Vec<u8>` violates that invariant for arbitrary input. Treat as Unix-correct,
    /// Windows-best-effort until we tighten the byte-form constructors.
    pub fn as_os_str(&self) -> &OsStr {
        // SAFETY: see the doc comment above. `from_encoded_bytes_unchecked` accepts either
        // `as_encoded_bytes` output or valid UTF-8 — both held for typical construction
        // sites (scanner via `as_encoded_bytes`, MCP layer via `&str`).
        unsafe { OsStr::from_encoded_bytes_unchecked(self.0.as_slice()) }
    }

    /// Convert to a `PathBuf` suitable for filesystem operations. Lossless on both Unix
    /// (raw bytes) and Windows (WTF-8 → UTF-16 round-trip via `OsStr::from_encoded_bytes`).
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(self.as_os_str())
    }
}

impl AsRef<OsStr> for RelPath {
    fn as_ref(&self) -> &OsStr {
        self.as_os_str()
    }
}

impl AsRef<Path> for RelPath {
    fn as_ref(&self) -> &Path {
        Path::new(self.as_os_str())
    }
}

impl fmt::Debug for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // BString's Debug already escapes invalid bytes as \xNN, which is what we want.
        write!(f, "RelPath({:?})", self.0)
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Use `ByteSlice::to_str_lossy` so invalid bytes become U+FFFD — readable for
        // humans, distinct from the discriminated `{"bytes": ...}` serde form.
        write!(f, "{}", self.0.as_slice().to_str_lossy())
    }
}

impl From<&str> for RelPath {
    fn from(s: &str) -> Self {
        Self(BString::from(s))
    }
}

impl From<String> for RelPath {
    fn from(s: String) -> Self {
        Self(BString::from(s))
    }
}

impl From<&String> for RelPath {
    fn from(s: &String) -> Self {
        Self(BString::from(s.as_str()))
    }
}

impl From<&RelPath> for RelPath {
    fn from(r: &RelPath) -> Self {
        r.clone()
    }
}

impl From<&[u8]> for RelPath {
    fn from(b: &[u8]) -> Self {
        Self(BString::from(b))
    }
}

impl From<Vec<u8>> for RelPath {
    fn from(v: Vec<u8>) -> Self {
        Self(BString::from(v))
    }
}

impl From<BString> for RelPath {
    fn from(b: BString) -> Self {
        Self(b)
    }
}

impl From<&BStr> for RelPath {
    fn from(b: &BStr) -> Self {
        Self(BString::from(<BStr as AsRef<[u8]>>::as_ref(b)))
    }
}

impl From<&Path> for RelPath {
    fn from(p: &Path) -> Self {
        Self(BString::from(p.as_os_str().as_encoded_bytes()))
    }
}

impl From<&OsStr> for RelPath {
    fn from(s: &OsStr) -> Self {
        Self(BString::from(s.as_encoded_bytes()))
    }
}

impl AsRef<[u8]> for RelPath {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl std::borrow::Borrow<BStr> for RelPath {
    fn borrow(&self) -> &BStr {
        self.0.as_bstr()
    }
}

// ─── serde — discriminated wire format ──────────────────────────────────────

impl Serialize for RelPath {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match std::str::from_utf8(self.0.as_slice()) {
            Ok(s) => ser.serialize_str(s),
            Err(_) => {
                use serde::ser::SerializeMap;
                let mut m = ser.serialize_map(Some(1))?;
                m.serialize_entry("bytes", self.0.as_slice())?;
                m.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for RelPath {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct RelPathVisitor;
        impl<'de> serde::de::Visitor<'de> for RelPathVisitor {
            type Value = RelPath;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a path string, raw bytes, or {\"bytes\": [u8, ...]} object")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(RelPath::from(v))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(RelPath::from(v))
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                Ok(RelPath::from(v))
            }
            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                Ok(RelPath::from(v))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut m: A,
            ) -> Result<Self::Value, A::Error> {
                let mut bytes: Option<Vec<u8>> = None;
                while let Some(key) = m.next_key::<String>()? {
                    if key == "bytes" {
                        bytes = Some(m.next_value()?);
                    } else {
                        let _: serde::de::IgnoredAny = m.next_value()?;
                    }
                }
                bytes
                    .map(RelPath::from)
                    .ok_or_else(|| serde::de::Error::missing_field("bytes"))
            }
        }
        de.deserialize_any(RelPathVisitor)
    }
}

// ─── schemars — used by rmcp request-param schema generation ─────────────────

impl rmcp::schemars::JsonSchema for RelPath {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RelPath".into()
    }
    fn json_schema(_: &mut rmcp::schemars::SchemaGenerator) -> rmcp::schemars::Schema {
        // Accept either a plain string (common case) or the discriminated
        // `{"bytes": [u8...]}` object that `Serialize` falls back to for non-UTF-8 bytes.
        rmcp::schemars::json_schema!({
            "oneOf": [
                { "type": "string" },
                {
                    "type": "object",
                    "properties": {
                        "bytes": {
                            "type": "array",
                            "items": { "type": "integer", "minimum": 0, "maximum": 255 }
                        }
                    },
                    "required": ["bytes"]
                }
            ]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_roundtrips_as_string() {
        let p = RelPath::from("src/main.rs");
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "\"src/main.rs\"");
        let back: RelPath = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn non_utf8_uses_discriminated_object() {
        // 0xff is invalid as a UTF-8 lead byte.
        let bytes: Vec<u8> = vec![b'f', 0xff, b'o', b'o', b'.', b'r', b's'];
        let p = RelPath::from(bytes.as_slice());
        assert!(!p.is_utf8());
        let json = serde_json::to_string(&p).unwrap();
        // Comes out as the `{"bytes": [...]}` form.
        assert!(json.starts_with("{\"bytes\":"), "got {json}");
        let back: RelPath = serde_json::from_str(&json).unwrap();
        assert_eq!(p.as_bytes(), back.as_bytes());
    }

    #[test]
    fn msgpack_roundtrips_both_shapes() {
        let utf8 = RelPath::from("a/b.rs");
        let bytes = rmp_serde::to_vec_named(&utf8).unwrap();
        let back: RelPath = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(utf8, back);

        let raw: Vec<u8> = vec![b'x', 0xfe, 0xfd];
        let bad = RelPath::from(raw.as_slice());
        let bytes = rmp_serde::to_vec_named(&bad).unwrap();
        let back: RelPath = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(bad.as_bytes(), back.as_bytes());
    }

    #[test]
    fn display_renders_lossy_for_invalid_utf8() {
        let bad = RelPath::from([b'a', 0xff, b'b'].as_slice());
        let s = format!("{bad}");
        // U+FFFD takes 3 bytes in UTF-8.
        assert!(s.contains('\u{FFFD}'), "got {s:?}");
    }

    #[test]
    fn borrow_by_bstr_works_for_btreemap_lookup() {
        use std::collections::BTreeMap;
        let mut m: BTreeMap<RelPath, u32> = BTreeMap::new();
        m.insert(RelPath::from("hello"), 1);
        let key: &BStr = b"hello".as_bstr();
        assert_eq!(m.get(key), Some(&1));
    }
}
