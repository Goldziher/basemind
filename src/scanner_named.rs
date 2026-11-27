//! Expansion of directory paths handed to the incremental pass: the directories a person or an
//! agent names for a rescan (`basemind rescan <dir>`, `index_rescan {"paths":["src/"]}`), and the
//! directory paths a watcher reports (a rename or move of a directory is one event on the
//! directory, never one per child). The incremental pass takes files: a directory handed to it as
//! one path is neither indexable nor a key in the index, so it would scan nothing and report
//! success.

use std::path::{Path, PathBuf};

use ahash::{AHashMap, AHashSet};

use crate::config::Config;
use crate::scanner_filter::{Filters, ignore_walk_builder};
use crate::store::Index;
use crate::store_key_order::KeyOrder;

/// Replace each directory in `paths` with the files a full scan would visit under it, plus every
/// file or document the index holds under it (so files deleted beneath it are pruned). A directory
/// that is gone from disk is expanded from the index alone. Files pass through unchanged.
///
/// A watcher batch can name thousands of paths, so only paths that may be directories are looked
/// up: an existing regular file, and a missing path the index holds under its own key, need none.
/// A lookup is a key range of `keys`, so a missing path no directory of the index starts with costs
/// no pass over the index.
pub(crate) fn expand_named_dirs(
    root: &Path,
    index: &Index,
    keys: &KeyOrder,
    config: &Config,
    filters: &Filters,
    paths: &[PathBuf],
) -> Vec<PathBuf> {
    let mut seen: AHashSet<PathBuf> = AHashSet::with_capacity(paths.len());
    let mut out = Vec::with_capacity(paths.len());
    let mut push = |path: PathBuf, out: &mut Vec<PathBuf>| {
        if seen.insert(path.clone()) {
            out.push(path);
        }
    };
    // Directory-like paths, by repo-relative key: whether the directory is on disk.
    let mut dirs: AHashMap<String, bool> = AHashMap::new();
    for abs in paths {
        let Some(rel) = rel_key(root, abs) else {
            continue;
        };
        match std::fs::symlink_metadata(abs).ok() {
            Some(meta) if meta.is_dir() => {
                dirs.insert(rel, true);
            }
            Some(_) => {}
            // Gone from disk: it may have been a directory unless the index holds it as a file.
            None if !is_indexed_key(index, &rel) => {
                dirs.entry(rel).or_insert(false);
            }
            None => {}
        }
    }
    let beneath = indexed_under(keys, &dirs);
    for abs in paths {
        let Some(rel) = rel_key(root, abs).filter(|rel| dirs.contains_key(rel)) else {
            push(abs.clone(), &mut out);
            continue;
        };
        let on_disk_dir = dirs[&rel];
        let indexed = beneath.get(rel.as_str());
        if !on_disk_dir && indexed.is_none() {
            push(abs.clone(), &mut out);
            continue;
        }
        if on_disk_dir {
            let pruner = filters.dir_pruner(Some(root));
            let walker = ignore_walk_builder(abs, config.scan.respect_gitignore, config.scan.follow_symlinks)
                .filter_entry(move |dent| pruner.keep(dent))
                .build();
            for dent in walker.flatten() {
                if dent.file_type().is_some_and(|kind| kind.is_file()) {
                    push(dent.into_path(), &mut out);
                }
            }
        }
        for key in indexed.into_iter().flatten() {
            push(root.join(key), &mut out);
        }
    }
    out
}

fn rel_key(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/");
    let rel = rel.trim_end_matches('/');
    (!rel.is_empty()).then(|| rel.to_string())
}

fn is_indexed_key(index: &Index, rel: &str) -> bool {
    let key = bstr::BStr::new(rel.as_bytes());
    index.files.contains_key(key) || index.doc_files.contains_key(key) || index.doc_tombstones.contains_key(key)
}

/// Every code file and document key the index holds beneath each of `dirs`, by key-range lookup
/// in `keys`: the cost follows the entries found, not the size of the index.
fn indexed_under<'k>(keys: &'k KeyOrder, dirs: &'k AHashMap<String, bool>) -> AHashMap<&'k str, Vec<&'k str>> {
    let mut out: AHashMap<&str, Vec<&str>> = AHashMap::new();
    for dir in dirs.keys() {
        let beneath: Vec<&str> = keys
            .under(dir)
            .filter_map(|key| key.as_str())
            // A dependency's documentation has no file under the tree, so no directory holds it.
            .filter(|key| !crate::deps_docs::is_virtual(key))
            .collect();
        if !beneath.is_empty() {
            out.insert(dir.as_str(), beneath);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand(root: &Path, index: &Index, named: &str) -> Vec<String> {
        let config = crate::config::default_for_root(root);
        let filters = Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("filters");
        let mut got: Vec<String> = expand_named_dirs(
            root,
            index,
            &KeyOrder::of(index),
            &config,
            &filters,
            &[root.join(named)],
        )
        .iter()
        .filter_map(|abs| rel_key(root, abs))
        .collect();
        got.sort();
        got
    }

    #[test]
    fn should_expand_a_directory_into_its_files_and_its_indexed_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src/deep")).expect("mkdir");
        std::fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").expect("write");
        std::fs::write(dir.path().join("src/deep/b.rs"), "fn b() {}\n").expect("write");
        std::fs::write(dir.path().join("top.rs"), "fn t() {}\n").expect("write");
        let mut index = Index::empty();
        let entry = crate::store::FileEntry {
            hash_hex: String::new(),
            language: "rust".to_string(),
            size_bytes: 0,
            mtime: 0,
            extract_epoch: 0,
            content_key: None,
            lines: 0,
        };
        index.files.insert(crate::path::RelPath::from("src/gone.rs"), entry);

        assert_eq!(
            expand(dir.path(), &index, "src"),
            ["src/a.rs", "src/deep/b.rs", "src/gone.rs"]
        );
        assert_eq!(expand(dir.path(), &index, "top.rs"), ["top.rs"]);

        std::fs::remove_dir_all(dir.path().join("src")).expect("rm");
        assert_eq!(expand(dir.path(), &index, "src"), ["src/gone.rs"]);
        assert_eq!(expand(dir.path(), &index, "nowhere"), ["nowhere"]);
    }

    #[test]
    fn should_drop_every_entry_under_a_removed_directory_and_only_those() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut index = Index::empty();
        for key in ["gone/a.rs", "gone/deep/b.rs", "gone.rs", "gone-two/c.rs", "gonetwo.rs"] {
            index.files.insert(
                crate::path::RelPath::from(key),
                crate::store::FileEntry {
                    hash_hex: String::new(),
                    language: "rust".to_string(),
                    size_bytes: 0,
                    mtime: 0,
                    extract_epoch: 0,
                    content_key: None,
                    lines: 0,
                },
            );
        }
        index.doc_tombstones.insert(
            crate::path::RelPath::from("gone/doc.pdf"),
            crate::store_doc_tombstone::DocTombstone {
                hash_hex: String::new(),
                guard_digest: String::new(),
                timeout_secs: 1,
            },
        );

        assert_eq!(
            expand(dir.path(), &index, "gone"),
            ["gone/a.rs", "gone/deep/b.rs", "gone/doc.pdf"]
        );
        assert_eq!(expand(dir.path(), &index, "gone.rs"), ["gone.rs"]);
        assert_eq!(expand(dir.path(), &index, "gon"), ["gon"]);
    }
}
