//! Wire types for forwarding precise resolved-reference reads from a `daemon_writer` serve to the
//! machine daemon.
//!
//! Under Seam B a comms-build `serve` opens its store read-only (no fjall index), so the cross-file
//! resolved-reference reverse index (`refs_by_def` / `refs_by_path`) — which lives ONLY in fjall —
//! is unreachable serve-side; the per-file `.rref` blobs carry intra-file edges only. Precise
//! cross-file `find_callers` / `goto_definition` (`resolved: true`) would therefore silently degrade
//! to the name-based fallback on every daemon-backed session. Serve forwards the lookup here; the
//! daemon — the sole fjall writer, holding the index — answers from `refs_by_def` / `refs_by_path`.
//!
//! Both types are float-free, so `Eq` is derivable (unlike the memory wire types).

#![cfg(all(feature = "comms", any(unix, windows)))]

use serde::{Deserialize, Serialize};

use crate::path::RelPath;

/// A precise resolved-reference lookup forwarded to the daemon. Answered from the workspace's
/// read-write fjall index (the daemon opens each workspace read-write as the sole writer), so the
/// reply carries the full intra + cross-file edge set the read-only serve cannot see.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolvedRefQuery {
    /// Every resolved use of the definition at `(def_path, def_start)` — backs `find_callers`.
    ReferencesTo { def_path: RelPath, def_start: u32 },
    /// The definition the use at `(use_path, use_start)` binds to — backs `goto_definition`.
    DefinitionOf { use_path: RelPath, use_start: u32 },
}

/// The daemon's answer to a [`ResolvedRefQuery`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolvedRefResult {
    /// Reply to [`ResolvedRefQuery::ReferencesTo`]: each resolved `(use_path, use_start)`.
    References(Vec<(RelPath, u32)>),
    /// Reply to [`ResolvedRefQuery::DefinitionOf`]: the resolved definition, if any.
    Definition(Option<ResolvedDefinition>),
}

/// A definition the daemon resolved, with what the asking session needs to tell whether the edge
/// belongs to the snapshot it reads: the daemon reads its read-write index, which a rescan updates
/// file by file before its resolve pass rewrites the cross-file edges.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedDefinition {
    /// The file the definition is in.
    pub path: RelPath,
    /// The definition's start byte in that file.
    pub start: u32,
    /// The content hash the daemon's index holds for `path`, when it holds the file.
    pub landing_hash: Option<String>,
    /// Whether the daemon's last resolve pass committed every edge. While one runs, an edge can
    /// still point into the content a file had before the rescan.
    pub settled: bool,
}

impl ResolvedDefinition {
    /// The definition as `(path, start)` when it belongs to a snapshot whose index holds
    /// `session_hash` for the landing file: the daemon's pass is settled and both name one
    /// content. Anything else is an edge from another snapshot, refused rather than served.
    pub fn for_snapshot(self, session_hash: Option<&str>) -> Option<(RelPath, u32)> {
        let same_content = self.landing_hash.is_some() && self.landing_hash.as_deref() == session_hash;
        (self.settled && same_content).then_some((self.path, self.start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(landing_hash: Option<&str>, settled: bool) -> ResolvedDefinition {
        ResolvedDefinition {
            path: RelPath::from("src/lib.rs"),
            start: 42,
            landing_hash: landing_hash.map(str::to_owned),
            settled,
        }
    }

    #[test]
    fn an_edge_of_the_sessions_snapshot_is_served() {
        assert_eq!(
            definition(Some("abc"), true).for_snapshot(Some("abc")),
            Some((RelPath::from("src/lib.rs"), 42))
        );
    }

    #[test]
    fn an_edge_into_other_content_is_refused() {
        assert_eq!(
            definition(Some("old"), true).for_snapshot(Some("new")),
            None,
            "the session rescanned first"
        );
        assert_eq!(
            definition(Some("new"), true).for_snapshot(Some("old")),
            None,
            "the daemon rescanned first"
        );
        assert_eq!(
            definition(None, true).for_snapshot(Some("abc")),
            None,
            "the daemon no longer holds the file"
        );
        assert_eq!(
            definition(Some("abc"), true).for_snapshot(None),
            None,
            "the session does not hold it"
        );
    }

    #[test]
    fn an_edge_read_while_the_resolve_pass_runs_is_refused() {
        assert_eq!(definition(Some("abc"), false).for_snapshot(Some("abc")), None);
    }

    #[test]
    fn a_definition_crosses_the_wire_whole() {
        let reply = ResolvedRefResult::Definition(Some(definition(Some("abc"), true)));
        let bytes = rmp_serde::to_vec_named(&reply).expect("encode");
        assert_eq!(
            rmp_serde::from_slice::<ResolvedRefResult>(&bytes).expect("decode"),
            reply
        );
    }
}
