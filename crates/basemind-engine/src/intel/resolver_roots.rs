//! Source-root discovery shared by the Python and Java resolvers.
//!
//! A repository of several projects has one source root per project, not one at the repository
//! root. A project is a directory holding a manifest (`pyproject.toml`, `pom.xml`, ...); its roots
//! are the conventional source directories below it. Each root records the top-level names it
//! holds, so an import is searched only in the roots that can hold it.

use std::collections::BTreeSet;
use std::path::Path;

use ahash::AHashSet;

/// A directory modules are imported from, and the top-level names it holds.
pub(super) struct SourceRoot {
    /// Repo-relative, forward-slashed.
    dir: String,
    names: AHashSet<String>,
}

/// The source roots of every project of the repository below `root`: for each directory holding a
/// file named in `manifests`, each of `layouts` (relative to the project, `""` for the project
/// directory itself). A root in `skip` is left out, for a caller that searches it anyway. `names`
/// lists what one root holds at its top level.
pub(super) fn discover(
    root: &Path,
    manifests: &[&str],
    layouts: &[&str],
    skip: &[&str],
    names: fn(&Path) -> AHashSet<String>,
) -> Vec<SourceRoot> {
    let projects: BTreeSet<String> = ignore::WalkBuilder::new(root)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter(|entry| entry.file_name().to_str().is_some_and(|name| manifests.contains(&name)))
        .filter_map(|entry| {
            let dir = entry.path().parent()?.strip_prefix(root).ok()?.to_str()?;
            Some(dir.replace('\\', "/"))
        })
        .collect();
    let dirs: BTreeSet<String> = projects
        .iter()
        .flat_map(|project| layouts.iter().map(move |layout| format!("{project}/{layout}")))
        .map(|dir| dir.trim_matches('/').to_string())
        .filter(|dir| !skip.contains(&dir.as_str()))
        .collect();
    dirs.into_iter()
        .map(|dir| SourceRoot {
            names: names(&root.join(&dir)),
            dir,
        })
        .collect()
}

/// The roots that hold `first`, nearest to `importer_rel` first: the more leading directories a
/// root shares with the importer's, the nearer it is, ties broken by path.
pub(super) fn nearest<'a>(roots: &'a [SourceRoot], importer_rel: &str, first: &str) -> impl Iterator<Item = &'a str> {
    let importer_dir: Vec<&str> = importer_rel.split('/').collect();
    let importer_dir = &importer_dir[..importer_dir.len() - 1];
    let mut near: Vec<(usize, &str)> = roots
        .iter()
        .filter(|source_root| source_root.names.contains(first))
        .map(|source_root| {
            let shared = importer_dir
                .iter()
                .zip(source_root.dir.split('/'))
                .take_while(|(left, right)| *left == right)
                .count();
            (shared, source_root.dir.as_str())
        })
        .collect();
    near.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(right.1)));
    near.into_iter().map(|(_, dir)| dir)
}
