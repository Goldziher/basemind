//! Resident term index behind the `symbols` and `dependents` modes.
//!
//! Both modes are substring sweeps over every file's symbol names / imports. Streaming the L1 blobs
//! for each query costs seconds on a large monorepo; this keeps only the searchable text (names and
//! import strings, no spans or signatures) in one compact allocation per file, so a sweep is a
//! `memmem` over contiguous bytes. Hits fetch their span and signature from the L1 cache by
//! `(path, symbol index)`, so only the returned page is ever decoded.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::extract::{FileMapL1, SymbolKind};
use crate::path::RelPath;

/// Separator between strings in a blob. A needle containing it can't be answered from the blob.
const SEP: u8 = 0;

/// Searchable text of one file.
pub(crate) struct FileTerms {
    /// Symbol names joined by [`SEP`], in outline order.
    names: Box<[u8]>,
    /// End offset of each symbol's name within `names`, in outline order.
    name_ends: Box<[u32]>,
    kinds: Box<[SymbolKind]>,
    /// Every import's `module` and `raw`, joined by [`SEP`].
    imports: Box<[u8]>,
}

impl FileTerms {
    fn from_l1(l1: &FileMapL1) -> Self {
        let mut names = Vec::new();
        let mut name_ends = Vec::with_capacity(l1.symbols.len());
        let mut kinds = Vec::with_capacity(l1.symbols.len());
        for sym in &l1.symbols {
            names.extend_from_slice(sym.name.as_bytes());
            names.push(SEP);
            name_ends.push(names.len() as u32 - 1);
            kinds.push(sym.kind);
        }
        let mut imports = Vec::new();
        for imp in &l1.imports {
            if let Some(m) = &imp.module {
                imports.extend_from_slice(m.as_bytes());
                imports.push(SEP);
            }
            imports.extend_from_slice(imp.raw.as_bytes());
            imports.push(SEP);
        }
        Self {
            names: names.into(),
            name_ends: name_ends.into(),
            kinds: kinds.into(),
            imports: imports.into(),
        }
    }

    /// Indices (into the outline's `symbols`) of names containing the needle, in outline order.
    /// `kind` filters before the caller counts, matching the streaming sweep.
    pub(crate) fn matching_symbols<'a>(
        &'a self,
        finder: &'a memchr::memmem::Finder<'_>,
        kind: Option<SymbolKind>,
    ) -> impl Iterator<Item = usize> + 'a {
        let mut from = 0usize;
        let mut last_idx = 0usize;
        std::iter::from_fn(move || {
            loop {
                let pos = finder.find(self.names.get(from..)?)? + from;
                // The name holding `pos`: first entry whose end is past it.
                let idx = last_idx + self.name_ends[last_idx..].partition_point(|&end| (end as usize) <= pos);
                let end = *self.name_ends.get(idx)? as usize;
                from = end + 1;
                last_idx = idx + 1;
                if pos + finder.needle().len() > end {
                    continue;
                }
                if kind.is_some_and(|k| self.kinds[idx] != k) {
                    continue;
                }
                return Some(idx);
            }
        })
    }

    /// Whether any import's module or raw text contains the needle.
    pub(crate) fn mentions_import(&self, finder: &memchr::memmem::Finder<'_>) -> bool {
        !self.imports.is_empty() && finder.find(&self.imports).is_some()
    }

    fn heap_bytes(&self) -> usize {
        self.names.len() + self.name_ends.len() * 4 + self.kinds.len() + self.imports.len()
    }
}

/// Path-ordered terms of every indexed file.
#[derive(Default)]
pub(crate) struct TermIndex {
    files: BTreeMap<RelPath, Arc<FileTerms>>,
}

impl TermIndex {
    /// Project every outline the stream yields.
    pub(crate) fn build(stream: impl FnOnce(&mut dyn FnMut(&RelPath, &FileMapL1))) -> Self {
        let mut files = BTreeMap::new();
        stream(&mut |path, l1| {
            files.insert(path.clone(), Arc::new(FileTerms::from_l1(l1)));
        });
        Self { files }
    }

    /// A copy with `removed` dropped and `updated` re-projected from `load`. Unchanged files share
    /// their allocation with `self`.
    pub(crate) fn patched(
        &self,
        updated: &[RelPath],
        removed: &[RelPath],
        load: impl Fn(&RelPath) -> Option<Arc<FileMapL1>>,
    ) -> Self {
        let mut files = self.files.clone();
        for path in removed.iter().chain(updated) {
            files.remove(path);
        }
        for path in updated {
            if let Some(l1) = load(path) {
                files.insert(path.clone(), Arc::new(FileTerms::from_l1(&l1)));
            }
        }
        Self { files }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&RelPath, &FileTerms)> {
        self.files.iter().map(|(p, t)| (p, &**t))
    }

    /// Approximate resident bytes, for diagnostics.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.files
            .iter()
            .map(|(p, t)| t.heap_bytes() + p.as_bytes().len() + 64)
            .sum()
    }
}

#[cfg(test)]
#[path = "term_index_tests.rs"]
mod tests;
