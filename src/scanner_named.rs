//! Expansion of the directories a person or an agent names for a rescan (`basemind rescan <dir>`,
//! `index_rescan {"paths":["src/"]}`). The incremental pass takes files: a directory handed to it
//! as one path is neither indexable nor a key in the index, so it would scan nothing and report
//! success.

use std::path::{Path, PathBuf};

use ahash::AHashSet;

use crate::config::Config;
use crate::scanner_filter::{Filters, ignore_walk_builder};
use crate::store::Index;

/// Replace each directory in `paths` with the files a full scan would visit under it, plus every
/// file or document the index holds under it (so files deleted beneath it are pruned). A directory
/// that is gone from disk is expanded from the index alone. Files pass through unchanged.
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
    for abs in paths {
        let Some(rel) = rel_key(root, abs) else {
            push(abs.clone(), &mut out);
            continue;
        };
        let on_disk_dir = std::fs::symlink_metadata(abs).is_ok_and(|meta| meta.is_dir());
        let prefix = format!("{rel}/");
        let indexed_beneath = indexed_under(index, &prefix);
        if !on_disk_dir && (abs.exists() || indexed_beneath.is_empty()) {
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
        for key in indexed_beneath {
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

/// Every code file and document key the index holds beneath `prefix` (which ends in `/`).
fn indexed_under(index: &Index, prefix: &str) -> Vec<String> {
    index
        .files
        .keys()
        .chain(index.doc_files.keys())
        .chain(index.doc_tombstones.keys())
        .filter_map(|key| key.as_str().filter(|key| key.starts_with(prefix)).map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand(root: &Path, index: &Index, named: &str) -> Vec<String> {
        let config = crate::config::default_for_root(root);
        let filters = Filters::build(&config, Vec::new()).expect("filters");
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
