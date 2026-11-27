//! Which working-tree file stands behind an indexed path, and the refusals for a path that has
//! none: not indexed, a document, gone, or outside the repository. Every operation that reads a
//! file the index names goes through here, so each refusal reads the same from every tool.

use crate::engine::error::EngineError;
use crate::engine::workspace::Workspace;
use crate::path::RelPath;

/// The error for a caller-supplied path that leaves the repository.
pub(crate) fn outside_repository(tool: &str, path: &RelPath) -> EngineError {
    EngineError::invalid_argument(format!(
        "`{tool}`: `{path}` is outside this repository. Pass a repo-relative path with no `..`; locate one \
             with `find_files {{\"query\":\"…\"}}`."
    ))
}

/// Where the files of this workspace's index are read from.
#[derive(Clone, Copy)]
pub struct IndexedFiles<'a> {
    pub root: &'a std::path::Path,
    /// `scan.follow_symlinks` as the scan applied it: the index then holds links whose targets are
    /// outside the root, by the operator's grant.
    follow_symlinks: bool,
    /// `scan.max_file_bytes`: no indexed file is read whole past it.
    pub max_file_bytes: u64,
}

/// Why [`IndexedFiles::file`] did not name a file to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unreadable {
    /// The index holds no such path, so nothing on disk is consulted for it.
    NotIndexed,
    /// The path leaves the repository.
    Outside,
}

impl<'a> IndexedFiles<'a> {
    pub(crate) fn of(ws: &'a Workspace) -> Self {
        Self {
            root: &ws.root,
            follow_symlinks: ws.config.scan.follow_symlinks,
            max_file_bytes: ws.config.scan.max_file_bytes,
        }
    }

    /// A root whose scan did not follow symlinks.
    #[cfg(test)]
    pub(crate) fn contained(root: &'a std::path::Path) -> Self {
        Self {
            root,
            follow_symlinks: false,
            max_file_bytes: u64::MAX,
        }
    }

    /// The working-tree file behind `path`, for reading, when `store` indexes it.
    ///
    /// Membership is looked up here, not promised by the caller: the two relaxations below are
    /// only sound for a path the scan itself admitted. A `scan.extra_roots` file is keyed by its
    /// absolute path and lives outside the repository by design, so its key is read as is. Under
    /// `scan.follow_symlinks` the scan indexed whatever a link resolved to, so only the lexical
    /// check applies and the read follows the link as the scan (and `code_grep`) did. Otherwise
    /// the path is resolved and must stay inside the root, which also covers an indexed file
    /// replaced by a link since the scan.
    pub(crate) fn file(&self, store: &crate::store::Store, path: &RelPath) -> Result<std::path::PathBuf, Unreadable> {
        if store.lookup(path).is_none() && store.lookup_doc(path).is_none() {
            return Err(Unreadable::NotIndexed);
        }
        self.indexed_path(path).ok_or(Unreadable::Outside)
    }

    /// [`file`](Self::file) for a path the caller took from the index itself: where to read it,
    /// or `None` when it now leads out of the repository.
    pub(crate) fn indexed_path(&self, path: &RelPath) -> Option<std::path::PathBuf> {
        if path.is_external() {
            return Some(path.to_path_buf());
        }
        if self.follow_symlinks {
            crate::path::lexical_file(self.root, path).ok()
        } else {
            crate::path::contained_file(self.root, path).ok()
        }
    }
}

/// [`IndexedFiles::indexed_path`] for a sweep over the whole corpus.
///
/// Resolving every path would cost a `realpath` per file. A file leaves the repository through a
/// link in one of two places: one of its directories, or the file itself. Each distinct directory
/// is resolved once per sweep and remembered, and the file costs one `lstat`.
pub struct Sweep<'a> {
    files: IndexedFiles<'a>,
    /// The resolved root; `None` when it cannot be resolved, and nothing is then inside it.
    root: Option<std::path::PathBuf>,
    /// Directory, as joined to the root → whether it resolves to a place inside the root.
    directories: std::sync::RwLock<ahash::AHashMap<std::path::PathBuf, bool>>,
}

impl<'a> Sweep<'a> {
    pub fn new(files: IndexedFiles<'a>) -> Self {
        Self {
            files,
            root: files.root.canonicalize().ok(),
            directories: std::sync::RwLock::default(),
        }
    }

    /// `scan.max_file_bytes` of the swept index.
    pub(crate) fn max_file_bytes(&self) -> u64 {
        self.files.max_file_bytes
    }

    /// Where to read `path`, or `None` when it is gone or now leads out of the repository.
    pub(crate) fn path(&self, path: &RelPath) -> Option<std::path::PathBuf> {
        if path.is_external() || self.files.follow_symlinks {
            return self.files.indexed_path(path);
        }
        let joined = crate::path::lexical_file(self.files.root, path).ok()?;
        if !self.directory_is_inside(joined.parent()?) {
            return None;
        }
        match std::fs::symlink_metadata(&joined) {
            Ok(metadata) if metadata.file_type().is_symlink() => self.files.indexed_path(path),
            Ok(_) => Some(joined),
            Err(_) => None,
        }
    }

    fn directory_is_inside(&self, directory: &std::path::Path) -> bool {
        if let Some(inside) = self
            .directories
            .read()
            .ok()
            .and_then(|known| known.get(directory).copied())
        {
            return inside;
        }
        let inside = self.root.as_deref().is_some_and(|root| {
            directory
                .canonicalize()
                .is_ok_and(|resolved| resolved.starts_with(root))
        });
        if let Ok(mut known) = self.directories.write() {
            known.insert(directory.to_path_buf(), inside);
        }
        inside
    }
}

/// [`IndexedFiles::file`] with its refusal phrased for the caller of `tool`.
pub(crate) fn indexed_file(
    ws: &Workspace,
    store: &crate::store::Store,
    tool: &str,
    path: &RelPath,
) -> Result<std::path::PathBuf, EngineError> {
    IndexedFiles::of(ws).file(store, path).map_err(|refusal| match refusal {
        Unreadable::NotIndexed => not_indexed_in(store, &ws.root, tool, path),
        Unreadable::Outside => outside_repository(tool, path),
    })
}

/// Why `path` cannot be served from the code map, phrased as the next call to make.
///
/// "Not indexed" has three different fixes, and an agent that is only told the first word guesses:
/// a document is searched with `code_grep` or `docs_search`, a file that exists needs `index_rescan`,
/// and a path that does not exist needs `find_files`. Judged on the store's file map: the in-RAM map
/// is also empty before a lazy build.
pub(crate) fn not_indexed_in(
    store: &crate::store::Store,
    root: &std::path::Path,
    tool: &str,
    path: &RelPath,
) -> EngineError {
    not_indexed_because(root, tool, path, index_facts(store, path))
}

/// What the file map says about a path the code map could not serve.
#[derive(Default)]
struct IndexFacts {
    /// The index holds no file at all.
    empty: bool,
    /// The path is an indexed document.
    is_document: bool,
    /// The path is in the file map, yet its extraction blob is absent or cannot be read.
    blob_lost: bool,
}

fn index_facts(store: &crate::store::Store, path: &RelPath) -> IndexFacts {
    let blob_lost = store
        .lookup(path)
        .is_some_and(|entry| !matches!(store.read_l1_by_hex(&entry.hash_hex), Ok(Some(_))));
    IndexFacts {
        empty: store.index.files.is_empty() && store.index.doc_files.is_empty(),
        is_document: store.lookup_doc(path).is_some(),
        blob_lost,
    }
}

fn not_indexed_because(root: &std::path::Path, tool: &str, path: &RelPath, facts: IndexFacts) -> EngineError {
    let on_disk = crate::path::contained_file(root, path);
    let message = if facts.empty {
        format!("`{tool}`: {EMPTY_INDEX_FIX}")
    } else if on_disk.is_err() {
        return outside_repository(tool, path);
    } else if facts.is_document && is_text_document(path) {
        format!(
            "`{tool}`: `{path}` is a document, not code, so it has no outline or symbols. Find exact text in \
             it with `code_grep`, search it by meaning with `docs_search`, or read a line span with `code_read`."
        )
    } else if facts.is_document {
        format!(
            "`{tool}`: `{path}` is a document, not code, so it has no outline or symbols. Search it by meaning \
             with `docs_search`."
        )
    } else if facts.blob_lost {
        format!(
            "`{tool}`: `{path}` is indexed, but its cache entry is missing or damaged. Run \
             `index_rescan {{\"paths\":[\"{path}\"]}}` to rebuild it and retry."
        )
    } else if on_disk.is_ok_and(|file| file.is_file()) {
        format!(
            "`{tool}`: `{path}` exists but is not indexed. Run `index_rescan {{\"paths\":[\"{path}\"]}}` and retry; \
             if it is still missing the file is ignored or not a supported language."
        )
    } else {
        format!("`{tool}`: no file `{path}` in this repository. Locate it with `find_files {{\"query\":\"…\"}}`.")
    };
    EngineError::invalid_argument(message)
}

fn is_text_document(path: &RelPath) -> bool {
    path.as_str()
        .is_some_and(|path| crate::engine::grep_text::text_document_language(std::path::Path::new(path)).is_some())
}

/// What to do about an index with no files in it.
pub(crate) const EMPTY_INDEX_FIX: &str = "the index is empty: nothing has been scanned for this workspace. Run \
     `index_rescan {}` (or `basemind scan` in a shell) and retry.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_index_is_fixed_by_a_call_the_tool_accepts() {
        let notice = crate::engine::types::LifecycleNotice::empty_index();
        for text in [EMPTY_INDEX_FIX, notice.message] {
            assert!(text.contains("`index_rescan {}`"), "{text}");
            assert!(
                !text.contains("`full`") && !text.contains("\"full\""),
                "`index_rescan` takes no such argument: {text}"
            );
        }
    }

    #[test]
    fn a_document_is_sent_to_code_grep_for_exact_text_and_docs_search_by_meaning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let facts = IndexFacts {
            is_document: true,
            ..IndexFacts::default()
        };
        let error = not_indexed_because(dir.path(), "code_outline", &RelPath::from("README.md"), facts);
        assert_eq!(
            error.to_string(),
            "`code_outline`: `README.md` is a document, not code, so it has no outline or symbols. Find exact \
             text in it with `code_grep`, search it by meaning with `docs_search`, or read a line span with \
             `code_read`."
        );
    }

    /// `code_grep` does not search PDFs or Office files, so their guidance names `docs_search` alone.
    #[test]
    fn a_binary_document_is_sent_to_docs_search_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        for path in ["docs/report.pdf", "docs/plan.docx", "docs/budget.xlsx"] {
            let facts = IndexFacts {
                is_document: true,
                ..IndexFacts::default()
            };
            let error = not_indexed_because(dir.path(), "code_outline", &RelPath::from(path), facts);
            assert_eq!(
                error.to_string(),
                format!(
                    "`code_outline`: `{path}` is a document, not code, so it has no outline or symbols. Search it \
                     by meaning with `docs_search`."
                )
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_sweep_reads_nothing_through_a_directory_or_a_file_that_links_out_of_the_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(root.join("src/deep")).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(root.join("src/deep/a.rs"), b"a").expect("write");
        std::fs::write(outside.join("b.rs"), b"b").expect("write");
        std::fs::write(outside.join("secret.rs"), b"s").expect("write");
        std::os::unix::fs::symlink(&outside, root.join("linked")).expect("link directory");
        std::os::unix::fs::symlink(outside.join("secret.rs"), root.join("src/leaf.rs")).expect("link file");
        std::os::unix::fs::symlink(root.join("src/deep/a.rs"), root.join("src/inside.rs")).expect("link inside");

        let sweep = Sweep::new(IndexedFiles::contained(&root));
        assert_eq!(
            sweep.path(&RelPath::from("src/deep/a.rs")),
            Some(root.join("src/deep/a.rs"))
        );
        assert_eq!(sweep.path(&RelPath::from("linked/b.rs")), None, "a linked directory");
        assert_eq!(sweep.path(&RelPath::from("linked/absent.rs")), None);
        assert_eq!(sweep.path(&RelPath::from("src/leaf.rs")), None, "a linked file");
        assert!(
            sweep.path(&RelPath::from("src/inside.rs")).is_some(),
            "a link that stays inside"
        );
        assert_eq!(
            sweep.path(&RelPath::from("src/gone.rs")),
            None,
            "deleted since the scan"
        );
        assert_eq!(sweep.path(&RelPath::from("gone/a.rs")), None, "its directory is gone");

        let known = sweep.directories.read().expect("lock");
        assert_eq!(known.len(), 4, "each distinct directory is resolved once: {known:?}");
        assert_eq!(known.get(&root.join("linked")), Some(&false));
        assert_eq!(known.get(&root.join("src/deep")), Some(&true));
    }
}
