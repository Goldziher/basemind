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

    /// True when this key refers to a file outside the repository root — i.e. one indexed
    /// via `scan.extra_roots`. External files are keyed by their absolute (forward-slash) path,
    /// so an absolute-path key is the discriminator (see [`is_external_key`]).
    /// Git-history / blame tools use this to short-circuit paths that git cannot resolve.
    pub fn is_external(&self) -> bool {
        is_external_key(self.0.as_slice())
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
        write!(f, "RelPath({:?})", self.0)
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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

/// Normalize a user-supplied CLI path into the repo-relative key the index is
/// keyed by (scanner-produced: no leading `./`, never absolute, `/`-separated).
///
/// Rules:
/// - Absolute paths are made relative by stripping `repo_root`; a path that does
///   not live under `repo_root` yields `None`.
/// - A leading `./` is dropped; lone `.` components and redundant separators are
///   collapsed.
/// - `..` is honored only while it stays inside the repo; any `..` that would
///   escape the root boundary yields `None` (we never resolve across the root).
///
/// Returns the normalized key, or `None` when the path escapes / falls outside
/// the repository. An empty result (path resolves to the repo root itself) also
/// yields `None` since there is no file key for the root.
pub(crate) fn normalize_query_path(user_path: &str, repo_root: &std::path::Path) -> Option<String> {
    let path = std::path::Path::new(user_path);

    if path.is_absolute() {
        if let Ok(inside) = path.strip_prefix(repo_root) {
            return normalize_relative_components(inside);
        }
        return normalize_absolute_components(path);
    }
    normalize_relative_components(path)
}

/// Collapse a repo-relative path to a `/`-joined key, honoring `.` and repo-bounded `..`.
/// Returns `None` when the path escapes the root or resolves to the root itself.
fn normalize_relative_components(relative: &std::path::Path) -> Option<String> {
    use std::path::Component;

    let mut parts: Vec<&str> = Vec::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(os) => {
                parts.push(os.to_str()?);
            }
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Collapse an absolute (out-of-repo) path to a `/`-prefixed key for an `extra_roots` file.
/// `..` is resolved lexically and clamped at the filesystem root; returns `None` for a
/// non-UTF-8 component or a bare `/`.
fn normalize_absolute_components(path: &std::path::Path) -> Option<String> {
    use std::path::Component;

    let mut prefix: Option<&str> = None;
    let mut parts: Vec<&str> = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Prefix(p) => prefix = Some(p.as_os_str().to_str()?),
            Component::Normal(os) => parts.push(os.to_str()?),
            Component::ParentDir => {
                parts.pop();
            }
        }
    }

    if parts.is_empty() {
        return None;
    }
    Some(match prefix {
        Some(drive) => format!("{drive}/{}", parts.join("/")),
        None => format!("/{}", parts.join("/")),
    })
}

/// Whether a stored key names a file outside the repository root (a `scan.extra_roots` file).
/// External files are keyed by their absolute path in forward-slash form, so the key is absolute:
/// a leading `/` on POSIX, or a Windows drive prefix (`C:/…`). Repo-relative keys are always
/// relative, so this never matches them — the drive form is Windows-gated so a POSIX filename that
/// legitimately contains `:` can't be mistaken for a drive.
pub(crate) fn is_external_key(key: &[u8]) -> bool {
    match key.first() {
        Some(b'/') => true,
        #[cfg(windows)]
        Some(&c) if c.is_ascii_alphabetic() => key.get(1) == Some(&b':'),
        _ => false,
    }
}

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
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut m: A) -> Result<Self::Value, A::Error> {
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

impl rmcp::schemars::JsonSchema for RelPath {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RelPath".into()
    }
    fn json_schema(_: &mut rmcp::schemars::SchemaGenerator) -> rmcp::schemars::Schema {
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
        let bytes: Vec<u8> = vec![b'f', 0xff, b'o', b'o', b'.', b'r', b's'];
        let p = RelPath::from(bytes.as_slice());
        assert!(!p.is_utf8());
        let json = serde_json::to_string(&p).unwrap();
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
        assert!(s.contains('\u{FFFD}'), "got {s:?}");
    }

    #[test]
    fn normalize_absolute_inside_repo_becomes_relative() {
        let root = std::path::Path::new(if cfg!(windows) { r"C:\abs\repo" } else { "/abs/repo" });
        let input = root.join("src").join("foo.rs");
        assert_eq!(
            normalize_query_path(input.to_str().unwrap(), root),
            Some("src/foo.rs".to_string())
        );
    }

    #[test]
    fn normalize_dot_slash_prefix_is_stripped() {
        let root = std::path::Path::new("/abs/repo");
        assert_eq!(
            normalize_query_path("./src/foo.rs", root),
            Some("src/foo.rs".to_string())
        );
    }

    #[test]
    fn normalize_already_relative_is_unchanged() {
        let root = std::path::Path::new("/abs/repo");
        assert_eq!(normalize_query_path("src/foo.rs", root), Some("src/foo.rs".to_string()));
    }

    #[test]
    fn normalize_absolute_outside_repo_passes_through_as_external_key() {
        #[cfg(unix)]
        {
            let root = std::path::Path::new("/abs/repo");
            assert_eq!(
                normalize_query_path("/other/place/foo.rs", root),
                Some("/other/place/foo.rs".to_string())
            );
            assert_eq!(
                normalize_query_path("/other/sub/../place/foo.rs", root),
                Some("/other/place/foo.rs".to_string())
            );
            assert_eq!(normalize_query_path("/", root), None);
        }
        #[cfg(windows)]
        {
            let root = std::path::Path::new(r"C:\abs\repo");
            assert_eq!(
                normalize_query_path(r"C:\other\place\foo.rs", root),
                Some("C:/other/place/foo.rs".to_string())
            );
            assert_eq!(
                normalize_query_path(r"C:\other\sub\..\place\foo.rs", root),
                Some("C:/other/place/foo.rs".to_string())
            );
            assert_eq!(normalize_query_path(r"C:\", root), None);
        }
    }

    #[test]
    fn is_external_flags_absolute_keys_only() {
        assert!(RelPath::from("/abs/ext/foo.rs").is_external());
        assert!(!RelPath::from("src/foo.rs").is_external());
        assert!(!RelPath::from("").is_external());
        #[cfg(windows)]
        assert!(RelPath::from("C:/ext/foo.rs").is_external());
    }

    #[test]
    fn normalize_collapses_redundant_separators_and_dots() {
        let root = std::path::Path::new("/abs/repo");
        assert_eq!(
            normalize_query_path("src/./bar/foo.rs", root),
            Some("src/bar/foo.rs".to_string())
        );
    }

    #[test]
    fn normalize_parent_inside_repo_resolves() {
        let root = std::path::Path::new("/abs/repo");
        assert_eq!(
            normalize_query_path("src/sub/../foo.rs", root),
            Some("src/foo.rs".to_string())
        );
    }

    #[test]
    fn normalize_parent_escaping_root_is_none() {
        let root = std::path::Path::new("/abs/repo");
        assert_eq!(normalize_query_path("../outside.rs", root), None);
    }

    #[test]
    fn normalize_root_itself_is_none() {
        let root = std::path::Path::new("/abs/repo");
        assert_eq!(normalize_query_path("/abs/repo", root), None);
        assert_eq!(normalize_query_path(".", root), None);
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
