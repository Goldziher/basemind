//! The keys of the file, document and tombstone maps in sorted order, so the entries under a
//! directory are a key-range lookup rather than a pass over every map.

use std::collections::BTreeSet;
use std::ops::Bound;

use bstr::BStr;

use crate::path::RelPath;
use crate::store::Index;

/// Every key `Index.files`, `doc_files` and `doc_tombstones` hold, once. The [`crate::store::Store`]
/// mutators keep it in step with those maps, so mutate them through the store.
#[derive(Debug, Default)]
pub(crate) struct KeyOrder(BTreeSet<RelPath>);

impl KeyOrder {
    pub(crate) fn of(index: &Index) -> Self {
        Self(
            index
                .files
                .keys()
                .chain(index.doc_files.keys())
                .chain(index.doc_tombstones.keys())
                .cloned()
                .collect(),
        )
    }

    pub(crate) fn insert(&mut self, rel: &RelPath) {
        if !self.0.contains(rel) {
            self.0.insert(rel.clone());
        }
    }

    /// Drop `rel` once none of the maps of `index` holds it any longer.
    pub(crate) fn forget(&mut self, rel: &RelPath, index: &Index) {
        let key: &BStr = rel.as_bstr();
        if !(index.files.contains_key(key)
            || index.doc_files.contains_key(key)
            || index.doc_tombstones.contains_key(key))
        {
            self.0.remove(rel);
        }
    }

    /// The keys strictly beneath the directory `dir` (`dir/...`), in order.
    pub(crate) fn under<'a>(&'a self, dir: &str) -> impl Iterator<Item = &'a RelPath> + 'a {
        let prefix = format!("{dir}/").into_bytes();
        self.0
            .range::<RelPath, _>((Bound::Included(RelPath::new(prefix.clone())), Bound::Unbounded))
            .take_while(move |key| key.as_bytes().starts_with(&prefix))
    }

    #[cfg(test)]
    pub(crate) fn matches(&self, index: &Index) -> bool {
        self.0 == Self::of(index).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FileEntry;

    fn entry() -> FileEntry {
        FileEntry {
            hash_hex: String::new(),
            language: "rust".to_string(),
            size_bytes: 0,
            mtime: 0,
            extract_epoch: 0,
            content_key: None,
            lines: 0,
        }
    }

    fn under(order: &KeyOrder, dir: &str) -> Vec<String> {
        order.under(dir).map(|key| key.to_str_lossy().into_owned()).collect()
    }

    #[test]
    fn under_lists_only_the_keys_inside_the_directory() {
        let mut index = Index::empty();
        for key in ["a/b.rs", "a/c/d.rs", "a.rs", "a-b/x.rs", "ab/y.rs", "z/a/b.rs"] {
            index.files.insert(RelPath::from(key), entry());
        }
        let order = KeyOrder::of(&index);

        assert_eq!(under(&order, "a"), ["a/b.rs", "a/c/d.rs"]);
        assert_eq!(under(&order, "a/c"), ["a/c/d.rs"]);
        assert!(under(&order, "missing").is_empty());
    }

    #[test]
    fn a_key_stays_until_every_map_has_dropped_it() {
        let mut index = Index::empty();
        let rel = RelPath::from("d/x.pdf");
        index.files.insert(rel.clone(), entry());
        index.doc_tombstones.insert(
            rel.clone(),
            crate::store_doc_tombstone::DocTombstone {
                hash_hex: String::new(),
                guard_digest: String::new(),
                timeout_secs: 0,
            },
        );
        let mut order = KeyOrder::of(&index);

        index.files.remove(rel.as_bstr());
        order.forget(&rel, &index);
        assert_eq!(under(&order, "d"), ["d/x.pdf"]);

        index.doc_tombstones.remove(rel.as_bstr());
        order.forget(&rel, &index);
        assert!(under(&order, "d").is_empty());
    }
}
