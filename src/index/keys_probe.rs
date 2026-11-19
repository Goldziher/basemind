//! Byte-level key encoding for the **import-probe** Fjall keyspaces that make the incremental
//! cross-file re-stitch exact and O(affected): `import_targets` and `importers_by_probe`.
//!
//! - `import_targets` records the stitch result per import binding:
//!   `u16:len(importer) ‖ importer ‖ local_start:u32_be  →  u16:len(def_path) ‖ def_path ‖ def_start:u32_be`.
//!   The reserved [`PROBE_MANIFEST_START`] row of an importer carries its probe manifest instead (the
//!   concatenated [`ProbeHash`]es of every `importers_by_probe` row it owns), so a re-stitch can
//!   delete exactly the rows it wrote without a third keyspace. The reserved [`FILE_EDGES_START`] row
//!   carries the importer's file-level import edges ([`FileEdges`]), the input of the import graph.
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

/// `local_start` value of the reserved file-edge row in `import_targets`, beside the manifest row:
/// like it, past any byte offset a source file can have.
pub const FILE_EDGES_START: u32 = u32::MAX - 1;

/// Most unresolved specifiers a file-edge row keeps; the count is kept whole.
pub const MAX_UNRESOLVED_SPECIFIERS: usize = 32;

/// One importer's file-level import edges: every indexed file one of its imports resolved to (the
/// first hop, before any re-export is followed), and what did not resolve to an indexed file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileEdges {
    /// One edge per target file, sorted by target.
    pub edges: Vec<FileEdge>,
    /// Imports with a non-relative specifier that did not resolve to an indexed file: a package,
    /// the standard library, or an alias the resolver does not know.
    pub external: u32,
    /// Imports with a relative specifier that did not resolve to an indexed file.
    pub unresolved: u32,
    /// The distinct specifiers counted in `unresolved`, at most [`MAX_UNRESOLVED_SPECIFIERS`].
    pub unresolved_specifiers: Vec<String>,
}

/// An import edge from an importer to `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdge {
    pub target: RelPath,
    /// Byte offset of the first import of `target`: of a runtime import when there is one, else of a
    /// module declaration, else of a type-only import.
    pub local_start: u32,
    /// Every kind of import behind the edge.
    pub kinds: EdgeKinds,
}

impl FileEdge {
    /// Every import of `target` is type-only (`import type`): erased at runtime.
    pub fn type_only(&self) -> bool {
        self.kinds == EdgeKinds::TYPE_ONLY
    }
}

/// The kinds of import behind one [`FileEdge`], a bit set (the edge's flag byte). An edge belongs to
/// the runtime tier when it holds [`Self::IMPORT`] or [`Self::MODULE_ONLY`]; an edge made only of
/// [`Self::TYPE_ONLY`] or [`Self::MODULE_DECL`] imports is in that tier alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct EdgeKinds(u8);

impl EdgeKinds {
    /// An import that binds a name of the target (`import { x }`, `use crate::a::X`, a Go import).
    pub const IMPORT: Self = Self(0b0001);
    /// A type-only import (`import type`), erased at runtime.
    pub const TYPE_ONLY: Self = Self(0b0010);
    /// A runtime import that binds no export: a JS side-effect import, a namespace import or an
    /// `export … from` re-export.
    pub const MODULE_ONLY: Self = Self(0b0100);
    /// A Rust `mod x;` declaration: the parent module contains the child's file. Structural
    /// containment, not a use of it.
    pub const MODULE_DECL: Self = Self(0b1000);
    const ALL: u8 = 0b1111;

    /// The set's flag byte.
    pub fn bits(self) -> u8 {
        self.0
    }

    /// The set of a flag byte; `None` for an empty set or an unknown bit.
    pub fn from_bits(bits: u8) -> Option<Self> {
        (bits != 0 && bits & !Self::ALL == 0).then_some(Self(bits))
    }

    /// Whether every kind of `other` is in the set.
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether the set and `other` share a kind.
    pub fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Whether the edge exists at runtime: a binding or a module-only import.
    pub fn runtime(self) -> bool {
        self.intersects(Self::IMPORT.union(Self::MODULE_ONLY))
    }

    /// The set with every kind of `other` added.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Encode the file-edge value:
/// `external:u32_be ‖ unresolved:u32_be ‖ edges:u32_be ‖ edge* ‖ specifier*`, an edge being
/// `kinds:u8 ‖ u16:len(target) ‖ target ‖ local_start:u32_be` (`kinds` being [`EdgeKinds::bits`])
/// and a specifier `u16:len ‖ bytes`.
pub fn file_edges_value(edges: &FileEdges) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + edges.edges.len() * 32);
    out.extend_from_slice(&edges.external.to_be_bytes());
    out.extend_from_slice(&edges.unresolved.to_be_bytes());
    let count = u32::try_from(edges.edges.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&count.to_be_bytes());
    for edge in edges.edges.iter().take(count as usize) {
        out.push(edge.kinds.bits());
        let _ = write_len_prefixed(&mut out, edge.target.as_bytes());
        out.extend_from_slice(&edge.local_start.to_be_bytes());
    }
    for specifier in &edges.unresolved_specifiers {
        let _ = write_len_prefixed(&mut out, specifier.as_bytes());
    }
    out
}

/// Decode a file-edge row to `(importer, edges)`. `None` for every other `import_targets` row and
/// for malformed input.
pub fn parse_file_edges(key: &[u8], value: &[u8]) -> Option<(RelPath, FileEdges)> {
    let mut c = 0;
    let importer = read_len_prefixed_ref(key, &mut c)?;
    if key.get(c..)? != FILE_EDGES_START.to_be_bytes() {
        return None;
    }
    let read_u32 = |at: usize| Some(u32::from_be_bytes(value.get(at..at + 4)?.try_into().ok()?));
    let (external, unresolved, count) = (read_u32(0)?, read_u32(4)?, read_u32(8)?);
    let mut v = 12;
    let mut edges = Vec::new();
    for _ in 0..count {
        let kinds = EdgeKinds::from_bits(*value.get(v)?)?;
        v += 1;
        let target = RelPath::from(read_len_prefixed_ref(value, &mut v)?);
        let local_start = read_u32(v)?;
        v += 4;
        edges.push(FileEdge {
            target,
            local_start,
            kinds,
        });
    }
    let mut unresolved_specifiers = Vec::new();
    while v < value.len() {
        let specifier = read_len_prefixed_ref(value, &mut v)?;
        unresolved_specifiers.push(String::from_utf8_lossy(specifier).into_owned());
    }
    Some((
        RelPath::from(importer),
        FileEdges {
            edges,
            external,
            unresolved,
            unresolved_specifiers,
        },
    ))
}

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
/// for the manifest and file-edge rows and for malformed input.
pub fn parse_import_target(key: &[u8], value: &[u8]) -> Option<(RelPath, u32, RelPath, u32)> {
    let mut c = 0;
    let importer = read_len_prefixed_ref(key, &mut c)?;
    let local_start = u32::from_be_bytes(key.get(c..c + 4)?.try_into().ok()?);
    if local_start == PROBE_MANIFEST_START || local_start == FILE_EDGES_START {
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
    value.as_chunks::<PROBE_HASH_LEN>().0.iter().copied()
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
    fn file_edges_roundtrip_under_the_importer_prefix_and_are_not_a_binding() {
        let importer = RelPath::from("src/app.ts");
        let edges = FileEdges {
            edges: vec![
                FileEdge {
                    target: RelPath::from("src/a.ts"),
                    local_start: 9,
                    kinds: EdgeKinds::IMPORT.union(EdgeKinds::MODULE_ONLY),
                },
                FileEdge {
                    target: RelPath::from("src/b.rs"),
                    local_start: 20,
                    kinds: EdgeKinds::MODULE_DECL,
                },
                FileEdge {
                    target: RelPath::from("src/c.rs"),
                    local_start: 30,
                    kinds: EdgeKinds::MODULE_DECL.union(EdgeKinds::IMPORT),
                },
                FileEdge {
                    target: RelPath::from("src/types.ts"),
                    local_start: 40,
                    kinds: EdgeKinds::TYPE_ONLY,
                },
            ],
            external: 3,
            unresolved: 1,
            unresolved_specifiers: vec!["./gone".to_owned()],
        };
        let key = import_target(&importer, FILE_EDGES_START);
        let value = file_edges_value(&edges);
        assert!(key.starts_with(&import_targets_prefix(&importer)));
        assert_eq!(parse_file_edges(&key, &value), Some((importer.clone(), edges)));
        assert_eq!(parse_import_target(&key, &value), None);

        let empty = file_edges_value(&FileEdges::default());
        assert_eq!(
            parse_file_edges(&key, &empty),
            Some((importer.clone(), FileEdges::default()))
        );
        assert_eq!(parse_file_edges(&key, &value[..value.len() - 1]), None, "a cut value");
        let binding = import_target(&importer, 12);
        assert_eq!(parse_file_edges(&binding, &value), None, "a binding row");
        let manifest = import_target(&importer, PROBE_MANIFEST_START);
        assert_eq!(parse_file_edges(&manifest, &value), None, "the manifest row");
    }

    #[test]
    fn edge_kinds_roundtrip_through_the_flag_byte_and_name_their_tier() {
        let kinds = [
            EdgeKinds::IMPORT,
            EdgeKinds::TYPE_ONLY,
            EdgeKinds::MODULE_ONLY,
            EdgeKinds::MODULE_DECL,
        ];
        let mut bits: Vec<u8> = kinds.iter().map(|kind| kind.bits()).collect();
        bits.sort_unstable();
        bits.dedup();
        assert_eq!(bits.len(), kinds.len(), "each kind has its own bit");
        for kind in kinds {
            assert_eq!(EdgeKinds::from_bits(kind.bits()), Some(kind));
        }
        let all = kinds.iter().fold(EdgeKinds::default(), |set, kind| set.union(*kind));
        assert_eq!(EdgeKinds::from_bits(all.bits()), Some(all));
        assert_eq!(EdgeKinds::from_bits(0), None, "an empty set is malformed");
        assert_eq!(EdgeKinds::from_bits(0x10), None, "an unknown bit is malformed");

        assert!(EdgeKinds::IMPORT.runtime() && EdgeKinds::MODULE_ONLY.runtime());
        assert!(!EdgeKinds::TYPE_ONLY.runtime() && !EdgeKinds::MODULE_DECL.runtime());
        assert!(EdgeKinds::MODULE_DECL.union(EdgeKinds::IMPORT).runtime());

        let edge = |kinds| FileEdge {
            target: RelPath::from("x.rs"),
            local_start: 0,
            kinds,
        };
        assert!(edge(EdgeKinds::TYPE_ONLY).type_only());
        assert!(!edge(EdgeKinds::TYPE_ONLY.union(EdgeKinds::IMPORT)).type_only());
        assert!(!edge(EdgeKinds::MODULE_DECL).type_only());

        let importer = RelPath::from("src/lib.rs");
        let key = import_target(&importer, FILE_EDGES_START);
        let mut value = file_edges_value(&FileEdges {
            edges: vec![edge(EdgeKinds::MODULE_DECL)],
            ..FileEdges::default()
        });
        assert_eq!(
            parse_file_edges(&key, &value).unwrap().1.edges[0].kinds,
            EdgeKinds::MODULE_DECL
        );
        value[12] = 0;
        assert_eq!(parse_file_edges(&key, &value), None, "an edge with no kind");
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
