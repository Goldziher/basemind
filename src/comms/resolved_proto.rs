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
//! Both types are float-free, so `Eq` is derivable (unlike the memory/proposal wire types).

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
    /// Reply to [`ResolvedRefQuery::DefinitionOf`]: the resolved `(def_path, def_start)`, if any.
    Definition(Option<(RelPath, u32)>),
}
