//! Types of the `index_*` operations.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::LifecycleNotice;
use super::files::SizedFile;
use crate::path::RelPath;

/// Arguments of `index_status`: none.
#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IndexStatusParams {}

/// Arguments of `index_rescan`.
#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IndexRescanParams {
    /// Repo-relative files or directories to re-index (default: the whole workspace).
    #[serde(default)]
    pub paths: Option<Vec<String>>,
}

/// Response of `index_status`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StatusResponse {
    pub file_count: usize,
    /// Count of content-addressed blob files in `.basemind/blobs/` (one `.fm.msgpack` per
    /// indexed content hash). Reported alongside `file_count` so a lost/empty view index over
    /// live blobs is visible rather than silently reading `file_count: 0` (bug #10).
    pub blob_count: usize,
    /// One-line advisory, present only when the view index is empty but blobs exist on disk
    /// (index lost/wiped) — suggests a rescan. Absent for a populated or legitimately
    /// unscanned (no-blobs) view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// `true` when the code index cannot answer until a scan rebuilds it: an older basemind wrote
    /// it, or a rebuild was interrupted. `note` says how to fix it. Absent (false) otherwise.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rescan_needed: bool,
    /// `true` when a writer (a running `scan`/`rescan`/`watch`) currently holds the store
    /// lock, so this report was served *without* blocking on the in-progress rebuild. The
    /// index counts (`file_count`, `languages`) reflect the pre-rebuild state or are omitted;
    /// `blob_count` is still read fresh from disk. Absent (false) on the common uncontended
    /// path — status then reflects the fully-committed index.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rebuild_in_progress: bool,
    /// `true` when the workspace's last full scan was cancelled or aborted before covering it (daemon
    /// shutdown, idle reap, a full disk), so `file_count` and the queries reflect only the files
    /// completed so far, not the repository. A full rescan clears it. Absent (false) otherwise.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    /// The watcher pass waiting for one of the daemon's scan slots, which every hosted workspace
    /// shares: the batches merged into it and what it will scan. Until it runs, the edits it holds
    /// are not indexed. Absent when no pass waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_scan: Option<QueuedScan>,
    /// One-line reason the index is degraded: a partial scan (with its reason and progress) or a
    /// resolve pass that never completed, leaving resolved references and callers stale. Absent when
    /// the last scan finished cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<String>,
    /// `true` while this serve's boot-time initial scan (auto-scan of an empty index) is still
    /// running. A client seeing this should treat empty query results as "index not ready yet"
    /// and poll again, rather than "no matches". Absent (false) once the index is built.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub indexing: bool,
    /// Wall-clock duration of this serve's boot-time initial scan, in milliseconds, once complete.
    /// Reports index-build time separately from query time so the first query's latency is not
    /// conflated with the one-time indexing cost. Absent when no initial scan ran this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_build_ms: Option<u64>,
    /// `true` while this serve's boot-time in-RAM code-map preload is still running (the index exists
    /// on disk but is loading into memory). Like `indexing`, a client should treat empty/partial query
    /// results as "not ready yet" and retry shortly rather than "no matches". Absent once warm.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub warming: bool,
    /// Wall-clock duration of the boot-time preload in milliseconds, once complete. Absent while still
    /// warming or when the preload wasn't deferred this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_ms: Option<u64>,
    /// `true` while this serve's boot-time git-history sync runs. A `git_log` / `git_blame` cursor
    /// issued before it ends may be refused as stale. Absent once the sync is done.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub history_syncing: bool,
    /// Current lifecycle notice (warming / building / rescanning) with an actionable message, or absent
    /// when the server is fully ready. Mirrors the `notice` on every read-tool response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<LifecycleNotice>,
    /// The Linux working-tree watcher's inotify watches, and the directories the watch limit left
    /// unwatched. Absent where no per-directory watcher runs: macOS and Windows watch the tree with
    /// one recursive watch, and a read-only serve or the CLI watches nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watches: Option<WatchStatus>,
    /// The full scan running now for this workspace, in this process or another: its `phase`, the
    /// candidate files extracted (`files_done`) of those it works through (`files_total`), and
    /// `eta_secs`, the seconds extraction has left at the rate it has run so far. Absent when no
    /// full scan runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_progress: Option<crate::scan_evidence::ScanProgress>,
    /// The background embedding of documents: `done` of `total` documents owed their vectors by
    /// the scan have them, and `eta_secs`, the seconds the running pass has left at its own rate.
    /// Until it finishes `docs_search` answers from its keyword lane and says so; a process that
    /// finds the pass interrupted starts it again. Absent when no document owes vectors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embeddings: Option<crate::embed_progress::EmbeddingsStatus>,
    /// The from-scratch git-history build a one-shot scan handed to the background worker: `done`
    /// of `total` commits stored, and `eta_secs`, the seconds the running build has left at its own
    /// rate. Until it finishes the history tools answer from the live walk. Absent when no build is
    /// owed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_history_build: Option<crate::git_history::progress::HistoryBuildStatus>,
    pub total_size_bytes: u64,
    pub languages: BTreeMap<String, usize>,
    /// List-only files (`[scan] list_only`: lockfiles, archives, captures, fonts, data) counted per
    /// kind (`lockfile`, `archive`, `binary`, `data`). Known to exist, never read, and not in
    /// `file_count`. Absent when there are none, or while a writer holds the store.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub listed: BTreeMap<String, usize>,
    /// Indexed files per path role (`test`, `infra`, `generated`, `vendored`, `example`; see
    /// `[scan.roles]`), counted within `file_count`: a file with two roles counts under both. Only
    /// roles that mark a file appear. Absent when none does, or while a writer holds the store.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, usize>,
    /// Code files too long to read whole. Absent while a writer holds the store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub large_files: Option<LargeFiles>,
    /// The files most others import (runtime imports; at most five). Absent when no import
    /// resolver placed an edge, or while a writer holds the store.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub most_imported: Vec<crate::engine::types::overview::ImportedFile>,
    /// basemind's cache directory for this workspace: its index views, git cache and telemetry.
    pub cache_dir: String,
    /// The tree-sitter grammar cache, which the language pack owns and other tools share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grammar_dir: Option<String>,
    pub schema_version: u16,
    pub root: String,
    /// Forward-slash worktree roots of every submodule declared in `.gitmodules`. Always
    /// reported regardless of `scan.skip_submodules` — lets clients see the boundary the
    /// scanner respects (or didn't, when the knob is disabled). Empty for repos with no
    /// submodules and for non-repo serves.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub submodules: Vec<RelPath>,
    /// Dependencies `[documents.dependencies]` names whose documentation the last full scan did
    /// not index, each with its reason (`not_installed`, `not_granted`, `escapes_root`,
    /// `unsupported_source`, `no_version`, `over_max_packages`). Absent when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps_unavailable: Vec<crate::deps_docs::DepUnavailable>,
    /// Size and modification time (`<bytes>B@<unix secs>`, or `absent`) of the workspace's
    /// `basemind.toml` on disk right now. A changed value between two calls means the file was
    /// edited; a long-lived daemon reloads it on the next request for that workspace.
    pub config_stamp: String,
    /// Server-side handler latency in microseconds — the tool body's own execution (index / store
    /// lookup + response construction), excluding MCP transport, argument deserialization, and
    /// response serialization. Distinct from `index_build_ms` / `warm_ms`, which report the
    /// one-time boot-scan and preload costs. See [`crate::engine::timing`] for the contract.
    /// Current branch; absent outside a git repository or on a detached HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Full HEAD sha; absent outside a git repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_short_sha: Option<String>,
    #[serde(default)]
    pub elapsed_us: u64,
    /// The MCP server's bounded work classes when this report was made: each class's slots, how
    /// many run and how many wait. Absent when no server answered (an in-process engine call).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<AdmissionStatus>,
}

/// How the MCP server admits its expensive calls: `heavy` (`index_rescan`, `code_search`,
/// `docs_search`) and `medium` (whole-corpus reads: `code_grep`, the import-graph relations of
/// `code_refs`, the CLI's import-graph analyses) each have their own slots. A call that finds every
/// slot of its class taken waits up to `queue_wait_ms` and is then answered `server_busy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct AdmissionStatus {
    pub heavy: LaneStatus,
    pub medium: LaneStatus,
    pub queue_wait_ms: u64,
}

impl AdmissionStatus {
    /// The lanes of a process that has admitted nothing: every slot free, none waiting.
    pub fn idle(limits: crate::config::daemon::AdmissionLimits) -> Self {
        let lane = |limit| LaneStatus {
            limit,
            in_use: 0,
            queued: 0,
        };
        Self {
            heavy: lane(limits.heavy_concurrency),
            medium: lane(limits.medium_concurrency),
            queue_wait_ms: u64::try_from(limits.queue_wait.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

/// One work class's slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct LaneStatus {
    /// Calls of the class that may run at once.
    pub limit: usize,
    /// Calls of the class running now.
    pub in_use: usize,
    /// Calls of the class waiting for a slot.
    pub queued: usize,
}

/// The `watches` block of `index_status`.
#[derive(Debug, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub(crate) struct WatchStatus {
    /// Watches the watcher holds, one per watched directory.
    pub held: usize,
    /// Directories the inotify watch limit refused a watch: their edits are not indexed until a
    /// rescan. Absent (0) when every directory is watched.
    #[serde(default, skip_serializing_if = "crate::engine::types::is_zero")]
    pub unwatched: usize,
    /// The current `fs.inotify.max_user_watches`, the limit across all of the user's processes.
    /// Absent when it cannot be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// What to raise and how, present only when directories are unwatched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The code files of at least `threshold_lines` lines: how many there are and the largest of them.
/// `find_files` with `min_lines` lists them all.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct LargeFiles {
    pub threshold_lines: u32,
    pub count: usize,
    /// The longest few, largest first (ties by path).
    pub largest: Vec<SizedFile>,
}

/// The `queued_scan` block of `index_status`.
#[derive(Debug, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct QueuedScan {
    /// Watcher batches merged into the waiting pass.
    pub batches: usize,
    /// Distinct paths the pass will scan; 0 for a whole-tree pass.
    pub paths: usize,
    /// The pass rescans the whole tree, because the OS dropped watch events. Absent (false)
    /// otherwise.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub whole_tree: bool,
}

/// Response of `index_rescan`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RescanResponse {
    pub scanned: usize,
    pub updated: usize,
    /// Documents (PDF / Office / HTML / MDX — anything with no tree-sitter grammar) re-extracted and
    /// re-embedded this rescan.
    ///
    /// Reported separately from `updated`, which counts CODE-map updates only. Without this a
    /// document-only change reconciled to nothing at all: the file was scanned and genuinely
    /// refreshed, but landed in no reported bucket, so `scanned` exceeded the sum of the counts and
    /// `updated: 0` read as "rescan ignored my document" when the content had in fact been updated.
    pub docs_indexed: usize,
    /// Documents chunked by the linear fallback instead of xberg's markdown splitter because they
    /// exceeded `[documents] markdown_chunk_max_bytes` / `plain_text_chunk_max_bytes`.
    pub docs_degraded: usize,
    /// Subset of `docs_degraded`: structured extraction failed (parse error or security limit, e.g. a
    /// Helm template as YAML or a CSV past the cell limit) and the file was re-read as `text/plain`.
    pub docs_text_fallback: usize,
    /// Documents abandoned because extraction overran `[documents] extraction_timeout_secs`; retried
    /// on the next scan.
    pub doc_timeouts: usize,
    pub removed: usize,
    pub skipped_unchanged: usize,
    pub skipped_no_lang: usize,
    pub extract_failed: usize,
    pub elapsed_ms: u128,
    pub root: String,
}
