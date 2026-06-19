//! Scope resolution and room auto-join matching.
//!
//! An agent connecting from a working directory has a [`ScopeChain`]: its repo's normalised
//! remote (if any), its cwd, and every ancestor directory up to a boundary (`$HOME` or the
//! filesystem root). [`room_matches`] tests a [`RoomScope`] against that chain — this is what
//! makes nested repos and horizontal monorepos auto-join a shared workspace room.

use std::path::{Path, PathBuf};

use super::model::RoomScope;
use crate::git::Repo;

/// The scope context an agent presents when it connects. Built once per Hello / ListRooms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeChain {
    /// Normalised git remote of the agent's repo, if it is inside one.
    pub remote: Option<String>,
    /// The agent's current working directory (canonicalised when possible).
    pub cwd: PathBuf,
    /// `cwd` plus every ancestor directory up to the boundary, nearest-first.
    pub ancestors: Vec<PathBuf>,
}

/// Build the [`ScopeChain`] for an agent rooted at `cwd`, optionally inside `repo`.
///
/// The remote is derived via [`crate::git::scope_key`] (which prefers the normalised `origin`
/// URL and falls back to `path:<workdir>`); for comms we only treat a true remote as a
/// `Remote` scope match, so a `path:`-prefixed fallback is dropped here and the path-prefix
/// rooms carry the workspace identity instead.
///
/// Ancestors are walked up to `$HOME` inclusive, or the filesystem root when `$HOME` is unset
/// or `cwd` is outside it — this bounds the chain so a stray `/` room cannot vacuum every
/// agent on the machine (that is what [`RoomScope::Global`] is for, explicitly).
pub fn scope_chain(cwd: &Path, repo: Option<&Repo>) -> ScopeChain {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());

    let remote = repo.and_then(|r| {
        let key = crate::git::scope_key(r);
        // `scope_key` returns `path:<workdir>` when there is no origin remote. That is a
        // local-only fallback, not a shareable remote identity, so we do not treat it as a
        // `Remote` scope — path-prefix rooms cover the local-only case.
        if key.starts_with("path:") {
            None
        } else {
            Some(key)
        }
    });

    let boundary = home_boundary();
    let ancestors = ancestors_up_to(&cwd, boundary.as_deref());

    ScopeChain {
        remote,
        cwd,
        ancestors,
    }
}

/// Resolve the `$HOME` boundary directory, canonicalised. `None` when unset.
fn home_boundary() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|p| p.canonicalize().ok())
}

/// `cwd` plus ancestors, nearest-first, stopping after `boundary` (inclusive). When `cwd` is
/// not under `boundary`, walk to the filesystem root instead.
fn ancestors_up_to(cwd: &Path, boundary: Option<&Path>) -> Vec<PathBuf> {
    let under_boundary = match boundary {
        Some(b) => cwd.starts_with(b),
        None => false,
    };
    let mut out = Vec::new();
    for ancestor in cwd.ancestors() {
        out.push(ancestor.to_path_buf());
        if under_boundary && Some(ancestor) == boundary {
            break;
        }
    }
    out
}

/// True when an agent with `chain` should auto-join a room with `room_scope`.
///
/// * [`RoomScope::Remote`] matches when it equals the chain's remote.
/// * [`RoomScope::PathPrefix`] matches when the path is an ANCESTOR of (prefix of) the agent's
///   cwd — i.e. the room sits at or above the agent in the directory tree.
/// * [`RoomScope::Global`] always matches.
pub fn room_matches(room_scope: &RoomScope, chain: &ScopeChain) -> bool {
    match room_scope {
        RoomScope::Remote(remote) => chain.remote.as_deref() == Some(remote.as_str()),
        RoomScope::PathPrefix(prefix) => {
            let prefix = prefix.canonicalize().unwrap_or_else(|_| prefix.clone());
            chain.cwd.starts_with(&prefix)
        }
        RoomScope::Global => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(remote: Option<&str>, cwd: &str) -> ScopeChain {
        ScopeChain {
            remote: remote.map(|s| s.to_string()),
            cwd: PathBuf::from(cwd),
            ancestors: PathBuf::from(cwd)
                .ancestors()
                .map(|p| p.to_path_buf())
                .collect(),
        }
    }

    #[test]
    fn global_matches_everything() {
        assert!(room_matches(
            &RoomScope::Global,
            &chain(None, "/anywhere/at/all")
        ));
    }

    #[test]
    fn remote_matches_only_exact_remote() {
        let c = chain(Some("github.com/foo/bar"), "/work/bar");
        assert!(room_matches(
            &RoomScope::Remote("github.com/foo/bar".to_string()),
            &c
        ));
        assert!(!room_matches(
            &RoomScope::Remote("github.com/foo/other".to_string()),
            &c
        ));
    }

    #[test]
    fn remote_does_not_match_when_agent_has_no_remote() {
        let c = chain(None, "/work/bar");
        assert!(!room_matches(
            &RoomScope::Remote("github.com/foo/bar".to_string()),
            &c
        ));
    }

    #[test]
    fn path_prefix_matches_ancestor_of_cwd() {
        // Use a non-existent path so canonicalize falls back to the literal path; the
        // ancestor relationship is what we assert.
        let c = chain(None, "/home/u/workspace/monorepo/services/api");
        assert!(
            room_matches(
                &RoomScope::PathPrefix(PathBuf::from("/home/u/workspace/monorepo")),
                &c
            ),
            "a room at an ancestor dir should cover a nested agent"
        );
    }

    #[test]
    fn path_prefix_does_not_match_sibling_or_descendant_only() {
        let c = chain(None, "/home/u/workspace/monorepo");
        // A deeper path is NOT an ancestor of cwd → no match.
        assert!(!room_matches(
            &RoomScope::PathPrefix(PathBuf::from("/home/u/workspace/monorepo/services")),
            &c
        ));
        // A sibling tree → no match.
        assert!(!room_matches(
            &RoomScope::PathPrefix(PathBuf::from("/home/u/other")),
            &c
        ));
    }

    #[test]
    fn ancestors_stop_at_home_boundary() {
        let home = PathBuf::from("/home/u");
        let cwd = PathBuf::from("/home/u/a/b");
        let ancestors = ancestors_up_to(&cwd, Some(&home));
        assert!(ancestors.contains(&PathBuf::from("/home/u/a/b")));
        assert!(ancestors.contains(&home));
        assert!(
            !ancestors.contains(&PathBuf::from("/home")),
            "must not walk above the HOME boundary"
        );
    }
}
