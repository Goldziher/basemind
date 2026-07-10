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
        for imp in imports {
            let module_match = imp
                .module
                .as_deref()
                .is_some_and(|m| m == module || module_finder.find(m.as_bytes()).is_some());
            let raw_match = !module_match && module_finder.find(imp.raw.as_bytes()).is_some();
            if module_match || raw_match {
                out.push(path.as_ref().to_path_buf());
                break;
            }
        }
    }
    out.sort();
    out
}
