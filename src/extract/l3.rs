use std::path::{Path, PathBuf};

use super::Import;

/// Find files whose import list mentions `module` either as the exact module path
/// or as a substring of the raw import text.
///
/// Accepts a slice of `(path, imports)` rather than a HashMap so callers can pass
/// pre-sorted vectors (the MCP server preloads one) without paying for HashMap
/// construction or being locked into a specific hasher.
pub fn dependents_of<P: AsRef<Path>>(module: &str, index: &[(P, Vec<Import>)]) -> Vec<PathBuf> {
    let module_finder = memchr::memmem::Finder::new(module.as_bytes());
    let mut out = Vec::new();
    for (path, imports) in index {
        if imports_mention(module, &module_finder, imports) {
            out.push(path.as_ref().to_path_buf());
        }
    }
    out.sort();
    out
}

/// Whether one file's import list mentions `module` — the per-file predicate behind
/// [`dependents_of`].
///
/// Exposed separately so a caller that STREAMS the corpus (the MCP `dependents` mode, which reads
/// outlines one bounded chunk at a time) can apply the identical match without first materialising
/// every file's imports into a slice. `module_finder` is passed in rather than built here so the
/// substring automaton is compiled once per query, not once per file.
pub fn imports_mention(module: &str, module_finder: &memchr::memmem::Finder<'_>, imports: &[Import]) -> bool {
    imports.iter().any(|imp| {
        let module_match = imp
            .module
            .as_deref()
            .is_some_and(|m| m == module || module_finder.find(m.as_bytes()).is_some());
        module_match || module_finder.find(imp.raw.as_bytes()).is_some()
    })
}
