//! Scope context and path-glob discovery matching.
//!
//! An agent connecting from a working directory presents a [`ScopeChain`]: its repo's
//! normalised remote (if any) and its canonicalised cwd. Thread discovery by path is a
//! [`path_matches`] test of a thread's glob pattern against that cwd — this is what lets an
//! agent find a thread scoped to `src/**` or to a repo path without being an explicit member.

use std::path::{Path, PathBuf};

use globset::Glob;

use crate::git::Repo;

/// The scope context an agent presents when it connects. Built once per Hello / ThreadList.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeChain {
    /// Normalised git remote of the agent's repo, if it is inside one.
    pub remote: Option<String>,
    /// The agent's current working directory (canonicalised when possible).
    pub cwd: PathBuf,
}

/// Build the [`ScopeChain`] for an agent rooted at `cwd`, optionally inside `repo`.
///
/// The remote is derived via [`crate::git::scope_key`] (which prefers the normalised `origin`
/// URL and falls back to `path:<workdir>`); for comms we only treat a true remote as a remote
/// match, so a `path:`-prefixed fallback is dropped here.
pub fn scope_chain(cwd: &Path, repo: Option<&Repo>) -> ScopeChain {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let remote = repo.and_then(|r| {
        let key = crate::git::scope_key(r);
        if key.starts_with("path:") { None } else { Some(key) }
    });
    ScopeChain { remote, cwd }
}

/// True when an agent whose cwd is `cwd` should discover a thread whose path is `pattern`.
///
/// `pattern` is a path or GLOB matched with `globset`. Matching is tried three ways so both a
/// literal repo path and a repo-relative glob (`src/**`) resolve intuitively:
/// * the compiled glob is tested against the absolute `cwd`;
/// * and against every ancestor path component of `cwd` (so a glob naming an ancestor dir
///   still covers a nested agent);
/// * a non-glob literal also matches when it is a path prefix of `cwd` (the ancestor-room case).
///
/// A pattern that fails to compile as a glob never matches (returns `false`) rather than erroring —
/// discovery is best-effort and a malformed pattern should not fail a listing.
pub fn path_matches(pattern: &str, cwd: &Path) -> bool {
    let cwd_str = cwd.to_string_lossy();
    if let Ok(glob) = Glob::new(pattern) {
        let matcher = glob.compile_matcher();
        if matcher.is_match(cwd.as_os_str()) || matcher.is_match(cwd_str.as_ref()) {
            return true;
        }
        for ancestor in cwd.ancestors() {
            if matcher.is_match(ancestor.as_os_str()) {
                return true;
            }
        }
    }
    // Literal path-prefix fallback: a plain directory pattern covers agents at or below it.
    let literal = Path::new(pattern);
    let literal = literal.canonicalize().unwrap_or_else(|_| literal.to_path_buf());
    cwd.starts_with(&literal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_cwd() {
        let cwd = PathBuf::from("/home/u/repo/src/api");
        assert!(path_matches("**/src/**", &cwd));
        assert!(!path_matches("**/tests/**", &cwd));
    }

    #[test]
    fn literal_prefix_matches_nested_cwd() {
        let cwd = PathBuf::from("/home/u/workspace/monorepo/services/api");
        assert!(
            path_matches("/home/u/workspace/monorepo", &cwd),
            "a literal ancestor path should cover a nested agent"
        );
        assert!(!path_matches("/home/u/other", &cwd));
    }

    #[test]
    fn glob_ancestor_component_matches() {
        let cwd = PathBuf::from("/home/u/monorepo/services/api");
        assert!(
            path_matches("**/monorepo", &cwd),
            "a glob naming an ancestor dir matches"
        );
    }

    #[test]
    fn malformed_glob_never_panics() {
        let cwd = PathBuf::from("/tmp/x");
        assert!(!path_matches("[unterminated", &cwd));
    }
}
