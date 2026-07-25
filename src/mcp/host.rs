//! The in-process **host seam** between a daemon-hosted read stack and the daemon's own workspace
//! pool (the machine's sole fjall writer).
//!
//! A daemon-hosted connection ([`super::BasemindServer::from_shared`]) runs the same tool bodies as a
//! thin `daemon_writer` serve: `read_only` + `daemon_writer`, so every write / rescan / precise
//! resolved-reference read would normally FORWARD over a [`CommsClient`](crate::comms::client::CommsClient)
//! socket to the daemon — but *the daemon is dialing itself*. This trait lets those hosted call sites
//! reach the pool directly instead of looping back through the socket: the daemon builds the shared
//! read stack with a [`HostBackend`] handle (the pool), and each branch site prefers it over the
//! forwarding client.
//!
//! Methods are **synchronous** — they run under `spawn_blocking` / `block_on` at the call sites,
//! matching the pool's own blocking API — and return `Result<_, String>` so the seam names no
//! transport-specific error type. The forwarding layer stays intact for the FALLBACK path (a thin
//! `daemon_writer` serve with no in-process pool); only the hosted path uses this seam.
//!
//! DEFERRED: git-history is intentionally NOT part of this seam yet. The daemon-backed git-history
//! handle already works (the daemon is the sole holder of `git-history.fjall/`), so a hosted
//! connection keeps forwarding those ops via
//! [`CommsClient::git_history`](crate::comms::client::CommsClient::git_history) for now — routing it
//! in-process is a follow-up, not a correctness gap.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::path::{Path, PathBuf};

/// In-process access to the daemon's sole-writer workspace pool, used by a daemon-hosted read stack
/// instead of forwarding to the daemon over its own socket. Implemented by
/// [`WorkspacePool`](crate::comms::workspace_pool::WorkspacePool).
pub(crate) trait HostBackend: Send + Sync {
    /// Scan (or incrementally rescan) `root` directly through the pool. `paths` (with `full == false`)
    /// drives an incremental rescan; `None`/empty or `full` scans the whole working tree. `embed`
    /// requests the inline vector-fill pass. Returns the scan stats.
    fn host_rescan(
        &self,
        root: &Path,
        paths: Option<Vec<PathBuf>>,
        full: bool,
        embed: bool,
    ) -> Result<crate::scanner::ScanStats, String>;

    /// Answer a precise resolved-reference query against the workspace's read-write fjall index (the
    /// pool holds it as the sole writer, so the cross-file `refs_by_def` / `refs_by_path` edges a
    /// read-only serve cannot see are present).
    fn host_resolved_refs(
        &self,
        root: &Path,
        query: crate::comms::resolved_proto::ResolvedRefQuery,
    ) -> Result<crate::comms::resolved_proto::ResolvedRefResult, String>;

    /// Run a CORE memory operation against the workspace's read-write index. The pool's per-workspace
    /// store lock serializes same-workspace ops, so a `Put` read-modify-write is atomic without a
    /// per-key lock.
    #[cfg(feature = "memory")]
    fn host_memory(
        &self,
        root: &Path,
        scope: &str,
        op: crate::comms::memory_proto::MemoryOp,
    ) -> Result<crate::comms::memory_proto::MemoryOutcome, String>;

    /// Run a PROPOSAL governance operation against the workspace's read-write index. Same locking
    /// contract as [`host_memory`](Self::host_memory).
    #[cfg(feature = "memory")]
    fn host_governance(
        &self,
        root: &Path,
        scope: &str,
        op: crate::comms::proposals_proto::GovernanceOp,
    ) -> Result<crate::comms::proposals_proto::GovernanceOutcome, String>;
}
