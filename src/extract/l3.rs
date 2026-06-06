// L3: cross-file resolution.
//
// For v0 we ship a heuristic only: `dependents_of(module)` matches a string against
// the `module` field on imports, plus a substring fallback against the raw import text.
// Real per-language module resolvers land later.

use std::collections::HashMap;
use std::path::PathBuf;

use super::Import;

/// Find files whose import list mentions `module` either as the exact module path
/// or as a substring of the raw import text.
pub fn dependents_of(module: &str, index: &HashMap<PathBuf, Vec<Import>>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (path, imports) in index {
        for imp in imports {
            let matches = imp
                .module
                .as_deref()
                .is_some_and(|m| m == module || m.contains(module))
                || imp.raw.contains(module);
            if matches {
                out.push(path.clone());
                break;
            }
        }
    }
    out.sort();
    out
}
