use std::collections::BTreeMap;

use ahash::{AHashMap, AHashSet};
use serde::{Deserialize, Serialize};

use super::keys;
use super::view::Part;
use crate::path::RelPath;
use crate::store::FileEntry;

#[derive(Deserialize, Serialize)]
enum DeltaEntry {
    Added(FileEntry),
    Modified(FileEntry),
    Deleted,
}

#[derive(Deserialize, Serialize)]
struct DeltaManifest {
    format: u16,
    seq: u64,
    base_generation: u64,
    base_commit: String,
    entries: BTreeMap<RelPath, DeltaEntry>,
    restitched: AHashSet<RelPath>,
}

struct Shadow<'a> {
    delta: &'a DeltaManifest,
    dead_hashes: AHashSet<&'a str>,
}

impl DeltaManifest {
    fn shadow<'a>(
        &'a self,
        base_counts: &AHashMap<&str, usize>,
        base_entry: impl Fn(&RelPath) -> Option<&'a FileEntry>,
    ) -> Shadow<'a> {
        let mut shadowed_counts = AHashMap::<&str, usize>::new();
        let mut added_hashes = AHashSet::new();
        for (path, entry) in &self.entries {
            if let Some(base) = base_entry(path) {
                *shadowed_counts.entry(&base.hash_hex).or_default() += 1;
            }
            if let DeltaEntry::Added(file) | DeltaEntry::Modified(file) = entry {
                added_hashes.insert(file.hash_hex.as_str());
            }
        }
        let dead_hashes = shadowed_counts
            .into_iter()
            .filter(|(hash, count)| base_counts.get(hash) == Some(count) && !added_hashes.contains(hash))
            .map(|(hash, _)| hash)
            .collect();
        Shadow {
            delta: self,
            dead_hashes,
        }
    }
}

impl Shadow<'_> {
    fn hides(&self, part: Part, key: &[u8]) -> bool {
        let path = match part {
            Part::Meta | Part::GrepBloom => return false,
            Part::CodeBm25Postings => {
                return keys::parse_code_bm25_posting_chunk_id(key)
                    .and_then(|chunk| chunk.rsplit_once(':'))
                    .is_some_and(|(hash, _)| self.dead_hashes.contains(hash));
            }
            Part::SymbolsByName => keys::parse_symbol_by_name(key).map(|(_, _, path, _)| path),
            Part::CallsByCallee => keys::parse_call_by_callee(key).map(|(_, path, _)| path),
            Part::ImportsByModule => keys::parse_import_by_module(key).map(|(_, path, _)| path),
            Part::ImplementationsByTrait => keys::parse_impl_by_trait(key).map(|(_, _, path, _)| path),
            Part::RefsByDef => keys::parse_ref_by_def(key).map(|(_, _, path, _)| path),
            Part::ImportersByProbe => keys::parse_importer_by_probe(key),
            Part::SymbolsByPath
            | Part::CallsByPath
            | Part::ImportsByPath
            | Part::ImplementationsByPath
            | Part::RefsByPath
            | Part::ImportTargets
            | Part::CodeBm25ByPath => keys::read_len_prefixed_ref(key, &mut 0).map(RelPath::from),
        };
        path.is_some_and(|path| {
            self.delta.entries.contains_key(&path)
                || (matches!(
                    part,
                    Part::RefsByDef | Part::RefsByPath | Part::ImportTargets | Part::ImportersByProbe
                ) && self.delta.restitched.contains(&path))
        })
    }
}

#[test]
fn shadows_resolved_uses_and_retires_only_unheld_content() {
    let first = RelPath::from("first.rs");
    let second = RelPath::from("second.rs");
    let added = RelPath::from("added.rs");
    let file = |hash: &str| FileEntry {
        hash_hex: hash.into(),
        language: "rust".into(),
        size_bytes: 1,
        mtime: 0,
        extract_epoch: 1,
    };
    let base = file("same");
    let counts = AHashMap::from_iter([("same", 2)]);
    let mut delta = DeltaManifest {
        format: 1,
        seq: 17,
        base_generation: 3,
        base_commit: "fixture".to_string(),
        entries: BTreeMap::new(),
        restitched: AHashSet::new(),
    };
    delta.entries.insert(first.clone(), DeltaEntry::Deleted);
    delta.restitched.insert(second.clone());
    let lookup = |path: &RelPath| (path == &first || path == &second).then_some(&base);
    let shared = keys::code_bm25_posting("term", "same:0").unwrap();
    let shadow = delta.shadow(&counts, lookup);
    assert!(!shadow.hides(Part::CodeBm25Postings, &shared));
    assert!(!shadow.hides(Part::SymbolsByPath, &keys::symbol_by_path(&second, 0)));
    assert!(shadow.hides(Part::RefsByDef, &keys::ref_by_def(&added, 0, &second, 1)));

    delta.entries.insert(second.clone(), DeltaEntry::Modified(file("new")));
    assert!(delta.shadow(&counts, lookup).hides(Part::CodeBm25Postings, &shared));
    delta.entries.insert(added, DeltaEntry::Added(file("same")));
    let bytes = rmp_serde::to_vec_named(&delta).unwrap();
    let reopened: DeltaManifest = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!((reopened.format, reopened.seq, reopened.base_generation), (1, 17, 3));
    assert_eq!(reopened.base_commit, "fixture");
    assert!(!reopened.shadow(&counts, lookup).hides(Part::CodeBm25Postings, &shared));
}
