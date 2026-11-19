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

/// Replace each directory in `paths` with the files a full scan would visit under it, plus every
/// file or document the index holds under it (so files deleted beneath it are pruned). A directory
/// that is gone from disk is expanded from the index alone. Files pass through unchanged.
///
/// A watcher batch can name thousands of paths, so the index is read once for the whole batch,
/// and only for paths that may be directories: an existing regular file, and a missing path the
/// index holds under its own key, need no lookup.
pub(crate) fn expand_named_dirs(
    root: &Path,
    index: &Index,
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
    let beneath = indexed_under(index, &dirs);
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

/// Every code file and document key the index holds beneath each of `dirs`, in one pass over the
/// index: a key is filed under each of its ancestor directories that `dirs` names.
fn indexed_under<'k>(index: &'k Index, dirs: &AHashMap<String, bool>) -> AHashMap<&'k str, Vec<&'k str>> {
    let mut out: AHashMap<&str, Vec<&str>> = AHashMap::new();
    if dirs.is_empty() {
        return out;
    }
    let keys = index
        .files
        .keys()
        .chain(index.doc_files.keys())
        .chain(index.doc_tombstones.keys())
        .filter_map(|key| key.as_str())
        // A dependency's documentation has no file under the tree, so no directory holds it.
        .filter(|key| !crate::deps_docs::is_virtual(key));
    for key in keys {
        let mut end = key.len();
        while let Some(slash) = key[..end].rfind('/') {
            let ancestor = &key[..slash];
            if dirs.contains_key(ancestor) {
                out.entry(ancestor).or_default().push(key);
            }
            end = slash;
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
        let mut got: Vec<String> = expand_named_dirs(root, index, &config, &filters, &[root.join(named)])
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
}
