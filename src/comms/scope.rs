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
/// `pattern` is a path or GLOB matched with `globset`. Matching is tried four ways so both a
/// literal repo path and a repo-relative glob (`src/**`) resolve intuitively:
/// * the compiled glob is tested against the absolute `cwd`;
/// * and against every ancestor path component of `cwd` (so a glob naming an ancestor dir
///   still covers a nested agent);
/// * a trailing `/**` is stripped and the base retried, so `<dir>/**` also covers `<dir>` itself;
/// * a non-glob literal also matches when it is a path prefix of `cwd` (the ancestor-room case).
///
/// A pattern that fails to compile as a glob never matches (returns `false`) rather than erroring —
/// discovery is best-effort and a malformed pattern should not fail a listing.
pub fn path_matches(pattern: &str, cwd: &Path) -> bool {
    if glob_covers(pattern, cwd) {
        return true;
    }
    // `<dir>/**` must also cover an agent sitting AT `<dir>`. globset's `**` requires at least one
    // path component after the slash, so the recursive form never matches its own base — which left
    // a thread scoped to `<repo>/**` invisible to an agent whose cwd IS `<repo>`, the most common
    // cwd there is, while still being visible from every subdirectory. Retrying the stripped base
    // covers the root without widening the glob (`/repo/**` still rejects `/repo2`).
    if let Some(base) = pattern.strip_suffix("/**")
        && !base.is_empty()
        && glob_covers(base, cwd)
    {
        return true;
    }
    // Literal path-prefix fallback: a plain directory pattern covers agents at or below it.
    let literal = Path::new(pattern);
    let literal = literal.canonicalize().unwrap_or_else(|_| literal.to_path_buf());
    cwd.starts_with(&literal)
}

/// Compile `pattern` as a glob and test it against `cwd` and each of `cwd`'s ancestors. A pattern
/// that does not compile matches nothing.
fn glob_covers(pattern: &str, cwd: &Path) -> bool {
    let Ok(glob) = Glob::new(pattern) else {
        return false;
    };
    let matcher = glob.compile_matcher();
    if matcher.is_match(cwd.as_os_str()) || matcher.is_match(cwd.to_string_lossy().as_ref()) {
        return true;
    }
    cwd.ancestors().any(|ancestor| matcher.is_match(ancestor.as_os_str()))
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
    fn recursive_glob_covers_an_agent_at_its_own_base_dir() {
        // `<repo>/**` is the natural way to scope a thread to a repo, and an agent's cwd usually IS
        // the repo root. globset's `**` requires at least one component after the slash, so the raw
        // pattern never matches its own base — leaving a thread scoped to `<repo>/**` invisible from
        // `<repo>` itself, the single most common cwd there is.
        assert!(
            path_matches("/home/u/repo/**", &PathBuf::from("/home/u/repo")),
            "a `<dir>/**` thread must be discoverable by an agent sitting AT `<dir>`"
        );
    }

    #[test]
    fn recursive_glob_still_covers_nested_and_still_excludes_siblings() {
        assert!(path_matches("/home/u/repo/**", &PathBuf::from("/home/u/repo/src/api")));
        assert!(
            !path_matches("/home/u/repo/**", &PathBuf::from("/home/u/repo2")),
            "matching the base dir must not widen the glob into sibling paths"
        );
    }

    #[test]
    fn recursive_glob_over_a_globbed_base_covers_that_base() {
        assert!(path_matches("**/monorepo/**", &PathBuf::from("/home/u/monorepo")));
    }

    #[test]
    fn malformed_glob_never_panics() {
        let cwd = PathBuf::from("/tmp/x");
        assert!(!path_matches("[unterminated", &cwd));
    }
}
