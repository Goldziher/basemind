//! Wire protocol between comms clients (serve / CLI / hooks) and the broker daemon.
//!
//! JSON-RPC-shaped: [`CommsRequest`] is an internally-tagged `method` + `params` enum and
//! [`CommsResponse`] / [`CommsNotification`] mirror it, so a future A2A HTTP front-end can
//! serialize the SAME enums to JSON and reuse this contract verbatim. Over the local IPC
//! transport the bodies are msgpack, but the serde shape is transport-agnostic.
//!
//! `proto_ver` negotiation in [`CommsRequest::Hello`] guards version skew: the daemon rejects
//! a client whose major protocol version it does not speak rather than silently
//! misreading a future request shape.

use serde::{Deserialize, Serialize};

use super::cursor::Cursor;
use super::ids::{AgentId, ThreadId};
use super::model::{AgentCard, MessageMeta, Thread};

/// The protocol version this build speaks. Bumped on any breaking change to the request /
/// response / notification shapes. Negotiated in [`CommsRequest::Hello`]. Bumped 1→2 for the
/// room→thread redesign.
pub const PROTO_VER: u32 = 2;

/// A request from a client to the broker. `method` selects the variant; `params` are the
/// flattened fields.
// `Eq` is intentionally dropped: the `Governance` variant carries a `ProposalRecord` (and, via the
// promote op, a `MemoryRecord`) whose git-derived `confidence` / `importance` are `f32`, which is not
// `Eq`. `Eq` was never load-bearing here (no HashMap/HashSet keys, no `assert_eq!` on the wire); only
// mpsc channels + serde use these enums, both of which need `PartialEq` at most.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum CommsRequest {
    /// First frame on a link: announce identity and negotiate protocol version. Carries the
    /// optional scope context (remote + cwd) used ONLY for path-glob discovery in `ThreadList` —
    /// it does NOT auto-join anything.
    Hello {
        /// The connecting agent's id.
        agent: AgentId,
        /// The protocol version the client speaks.
        proto_ver: u32,
        /// Normalised git remote of the agent's repo, if any.
        #[serde(default)]
        remote: Option<String>,
        /// The agent's current working directory, for path-glob discovery.
        #[serde(default)]
        cwd: Option<std::path::PathBuf>,
    },
    /// Register or update the agent's card.
    Register {
        /// The agent's self-described A2A card.
        card: AgentCard,
    },
    /// List known agents, optionally scoped to the members of one thread.
    ListAgents {
        /// Restrict to members of this thread when set.
        #[serde(default)]
        thread: Option<ThreadId>,
    },
    /// Start a new thread addressed by AT LEAST TWO of `subject` / `path` / `members`. The
    /// calling agent becomes the creator and an implicit member. Fewer than two dimensions is
    /// rejected. Returns the created [`Thread`].
    ThreadStart {
        /// Topic string.
        #[serde(default)]
        subject: Option<String>,
        /// Path or GLOB pattern for path-based discovery.
        #[serde(default)]
        path: Option<String>,
        /// Explicit additional members (the creator is added automatically).
        #[serde(default)]
        members: Vec<AgentId>,
    },
    /// Join an existing thread (durable membership; drives the inbox).
    ThreadJoin {
        /// The thread to join.
        thread: ThreadId,
    },
    /// Leave a thread the calling agent is a member of.
    ThreadLeave {
        /// The thread to leave.
        thread: ThreadId,
    },
    /// List threads DISCOVERABLE to the calling agent: those it is a member of, those whose path
    /// glob matches its cwd, or (when `subject_contains` is set) those whose subject contains the
    /// filter. NEVER all threads. Archived threads are excluded unless `include_archived`.
    ThreadList {
        /// Remote of the agent's repo, if any (reserved for future remote-scoped discovery).
        #[serde(default)]
        remote: Option<String>,
        /// The agent's cwd, used for path-glob discovery.
        #[serde(default)]
        cwd: Option<std::path::PathBuf>,
        /// Optional case-sensitive substring filter over thread subjects.
        #[serde(default)]
        subject_contains: Option<String>,
        /// When true, also return archived threads.
        #[serde(default)]
        include_archived: bool,
    },
    /// Post a message to a thread. Returns the new message id.
    ThreadPost {
        /// Target thread.
        thread: ThreadId,
        /// Subject line.
        subject: String,
        /// Free-form tags.
        #[serde(default)]
        tags: Vec<String>,
        /// Id of the message being replied to, for threading.
        #[serde(default)]
        reply_to: Option<String>,
        /// The message body bytes.
        body: Vec<u8>,
    },
    /// Read a thread's history, oldest-first, paginated. Returns front-matter only.
    ThreadHistory {
        /// The thread to read.
        thread: ThreadId,
        /// Resume token from a previous page.
        #[serde(default)]
        cursor: Option<Cursor>,
        /// Maximum messages to return.
        #[serde(default)]
        limit: Option<u32>,
        /// Absolute recency cutoff in microseconds since the unix epoch: only messages whose
        /// `ts_micros >= since_micros` are returned. `None` returns ALL history.
        #[serde(default)]
        since_micros: Option<i64>,
    },
    /// List the members of a thread.
    ThreadMembers {
        /// The thread whose members to list.
        thread: ThreadId,
    },
    /// Add a member to a thread. Only the creator may do this.
    ThreadAddMember {
        /// The thread to modify.
        thread: ThreadId,
        /// The agent to add.
        member: AgentId,
    },
    /// Remove a member from a thread. Only the creator may do this.
    ThreadRemoveMember {
        /// The thread to modify.
        thread: ThreadId,
        /// The agent to remove.
        member: AgentId,
    },
    /// Archive a thread. Only the creator (or a human via the CLI) may do this. Idempotent.
    ThreadArchive {
        /// The thread to archive.
        thread: ThreadId,
    },
    /// Fetch a single message's body by id.
    GetBody {
        /// The message id (the `id` field of a [`MessageMeta`]).
        message_id: String,
    },
    /// Read the calling agent's inbox: new messages across all JOINED threads.
    Inbox {
        /// Remote of the agent's repo (unused for inbox membership; kept for symmetry).
        #[serde(default)]
        remote: Option<String>,
        /// The agent's cwd (unused for inbox membership; kept for symmetry).
        #[serde(default)]
        cwd: Option<std::path::PathBuf>,
        /// Resume token from a previous page.
        #[serde(default)]
        cursor: Option<Cursor>,
        /// Maximum messages to return.
        #[serde(default)]
        limit: Option<u32>,
        /// When true, advance the agent's read cursors past the returned messages.
        #[serde(default)]
        mark_read: bool,
        /// Absolute recency cutoff in microseconds since the unix epoch: only messages whose
        /// `ts_micros >= since_micros` surface. `None` returns ALL unread.
        #[serde(default)]
        since_micros: Option<i64>,
    },
    /// Acknowledge inbox messages by ADVANCING the calling agent's per-thread read cursors. This
    /// never deletes from the shared append-only log nor affects any other agent — it only moves
    /// THIS agent's cursors forward (monotonic). Two modes, combinable:
    /// * `message_ids` — resolve each id to its `(thread, seq)`, then advance each thread's cursor
    ///   to the max acked seq in that thread.
    /// * `thread` + `to_seq` — advance that one thread's cursor straight to `to_seq`.
    AckInbox {
        /// Message ids to ack (mode a). Empty when only the bulk mode is used.
        #[serde(default)]
        message_ids: Vec<String>,
        /// Target thread for the bulk `to_seq` mode (mode b).
        #[serde(default)]
        thread: Option<ThreadId>,
        /// Advance `thread`'s cursor straight to this seq (mode b). Requires `thread`.
        #[serde(default)]
        to_seq: Option<u64>,
    },
    /// Open a notification stream for a thread (the link receives [`CommsNotification::Message`]
    /// for every subsequent post). Returns a subscription handle.
    Subscribe {
        /// The thread to stream.
        thread: ThreadId,
    },
    /// Cancel a notification stream opened by [`CommsRequest::Subscribe`].
    Unsubscribe {
        /// The subscription handle returned by `Subscribe`.
        sub: u64,
    },
    /// Scan or rescan a workspace. The daemon is the sole fjall writer; front-ends forward their
    /// writes here so concurrent read-only sessions never contend for the index lock.
    Rescan {
        /// Canonical workspace root (worktree root).
        root: std::path::PathBuf,
        /// Restrict to these paths (incremental). `None`/empty or `full` → full working-tree scan.
        #[serde(default)]
        paths: Option<Vec<std::path::PathBuf>>,
        /// Force a complete re-index (overrides `paths`).
        #[serde(default)]
        full: bool,
        /// Run the scan with [`EmbedMode::Inline`](crate::scanner::EmbedMode::Inline) so the daemon
        /// fills document + code-chunk vectors into LanceDB. Defaults to `false` (the fast
        /// `Deferred` code-map + keyword pass); front-ends request `true` for the detached
        /// vector-fill follow-up. `#[serde(default)]` keeps older front-ends wire-compatible.
        #[serde(default)]
        embed: bool,
    },
    /// Forward a CORE memory operation to the daemon (the sole fjall writer). The namespace
    /// (`vis_byte` / `owner`) and `scope` are resolved serve-side; the daemon runs the op against
    /// the workspace's read-write `memory_by_key` index. The vector half (LanceDB) stays on serve.
    #[cfg(feature = "memory")]
    Memory {
        /// Canonical workspace root (worktree root).
        root: std::path::PathBuf,
        /// Memory scope resolved serve-side (git scope key or `path:<root>`).
        scope: String,
        /// The operation to run.
        op: crate::comms::memory_proto::MemoryOp,
    },
    /// Forward a PROPOSAL governance operation to the daemon (the sole fjall writer). The `scope` is
    /// resolved serve-side; the daemon runs the fjall reads/writes against the workspace's read-write
    /// `proposals` (and, for a promote, `memory_by_key`) index. The compute halves — git-log mining,
    /// the audit verdict, and the LanceDB embed — stay on serve.
    #[cfg(feature = "memory")]
    Governance {
        /// Canonical workspace root (worktree root).
        root: std::path::PathBuf,
        /// Memory scope resolved serve-side (git scope key or `path:<root>`).
        scope: String,
        /// The operation to run.
        op: crate::comms::proposals_proto::GovernanceOp,
    },
    /// Forward a git-history operation to the daemon. `git-history.fjall/` is a fjall database, and
    /// fjall's directory lock is exclusive — so exactly ONE process may hold a repo's history index.
    /// Under this model that process is the daemon: it BUILDS the index ([`GitHistoryOp::Sync`],
    /// serialized per repo so N sessions produce one build) and answers the front-ends' history
    /// reads from it. A `daemon_writer` serve therefore never opens the database — building it there
    /// would both steal the daemon's lock and run a multi-GB, minutes-long history walk inside the
    /// process an agent is actively querying.
    ///
    /// [`GitHistoryOp`]: crate::git_history::proto::GitHistoryOp
    GitHistory {
        /// Canonical workspace root (worktree root), selecting the repo's index.
        root: std::path::PathBuf,
        /// The operation to run.
        op: crate::git_history::proto::GitHistoryOp,
    },
    /// Forward a precise resolved-reference read to the daemon. A `daemon_writer` serve holds no
    /// fjall index, so the cross-file `refs_by_def` / `refs_by_path` edges live only daemon-side;
    /// this fetches them so `find_callers` / `goto_definition` keep their precise (`resolved: true`)
    /// cross-file resolution instead of degrading to the name-based fallback. A pure read.
    ResolvedRefs {
        /// Canonical workspace root, selecting the daemon's hot workspace.
        root: std::path::PathBuf,
        /// The lookup to run against that workspace's read-write index.
        query: crate::comms::resolved_proto::ResolvedRefQuery,
    },
    /// Report the workspaces the daemon currently holds hot (drives the statusline).
    AccessedPaths,
    /// List every registered workspace in the machine registry (git + plain). Read-only.
    WorkspacesList,
    /// List the worktrees of a registered repo, by [`crate::registry::RepoId`]. Read-only.
    WorktreesList {
        /// The repo id (normalized remote URL or `path:<root>`) whose worktrees to list.
        repo_id: String,
    },
    /// List the local branches of a registered repo, by [`crate::registry::RepoId`]. Read-only.
    BranchesList {
        /// The repo id whose branches to list.
        repo_id: String,
    },
    /// Advisory-claim a worktree for a claimant. Returns whether the claim is now held by them.
    WorktreeClaim {
        /// The owning repo id.
        repo_id: String,
        /// The worktree name (`"(main)"` or the linked-worktree directory name).
        name: String,
        /// The claimant id (an agent/session id) taking the advisory claim.
        claimant: String,
    },
    /// Release an advisory worktree claim held by `claimant`.
    WorktreeRelease {
        /// The owning repo id.
        repo_id: String,
        /// The worktree name whose claim to release.
        name: String,
        /// The claimant id releasing its own claim.
        claimant: String,
    },
    /// Liveness probe. The daemon replies [`CommsResponse::Pong`].
    Ping,
    /// Ask the daemon to drain and stop. Used by `basemind comms stop`.
    Stop,
    /// Report daemon status (pid / version / uptime / thread + subscriber counts).
    Status,
}

/// A response from the broker to a [`CommsRequest`].
// `Eq` dropped for the same reason as [`CommsRequest`]: the `Governance` outcome carries an
// `f32`-bearing `ProposalRecord`. `PartialEq` is retained (used by tests + serde round-trips).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", content = "data", rename_all = "snake_case")]
pub enum CommsResponse {
    /// Reply to [`CommsRequest::Hello`]: the daemon's protocol version + accept/reject.
    Welcome {
        /// The protocol version the daemon speaks.
        proto_ver: u32,
        /// Daemon build version string.
        daemon_version: String,
    },
    /// Acknowledge a side-effecting request that returns no payload.
    Ok,
    /// Reply to [`CommsRequest::ListAgents`].
    Agents(Vec<super::model::AgentRecord>),
    /// Reply to [`CommsRequest::ThreadStart`] and thread lookups.
    Thread(Thread),
    /// Reply to [`CommsRequest::ThreadList`].
    Threads(Vec<Thread>),
    /// Reply to [`CommsRequest::ThreadMembers`].
    Members {
        /// The member agent ids of the thread.
        members: Vec<AgentId>,
    },
    /// Reply to [`CommsRequest::ThreadPost`]: the new message id.
    Posted {
        /// The id of the message just stored.
        message_id: String,
    },
    /// Reply to [`CommsRequest::ThreadHistory`].
    History {
        /// The page of front-matter records, each paired with its per-thread `seq`.
        messages: Vec<SeqMeta>,
        /// Resume token for the next page, when more remain.
        next_cursor: Option<Cursor>,
    },
    /// Reply to [`CommsRequest::Inbox`].
    Inbox {
        /// The page of front-matter records across joined threads, each with its per-thread `seq`.
        messages: Vec<SeqMeta>,
        /// Count of unread messages remaining after this page.
        unread: u32,
        /// Resume token for the next page, when more remain.
        next_cursor: Option<Cursor>,
    },
    /// Reply to [`CommsRequest::AckInbox`]: how many ids were acked and the new per-thread cursor
    /// values that advanced as a result.
    Acked {
        /// Number of message ids that resolved and were acked (excludes unknown ids; the bulk
        /// `to_seq` mode does not contribute to this count).
        acked: u32,
        /// `(thread, new_seq)` for each thread whose cursor advanced in this call.
        cursors_advanced: Vec<(String, u64)>,
    },
    /// Reply to [`CommsRequest::GetBody`].
    Body {
        /// The body bytes, or `None` when the message id is unknown.
        body: Option<Vec<u8>>,
    },
    /// Reply to [`CommsRequest::Subscribe`]: the subscription handle.
    Subscribed {
        /// The handle to pass to [`CommsRequest::Unsubscribe`].
        sub: u64,
    },
    /// Reply to [`CommsRequest::Ping`].
    Pong,
    /// Reply to [`CommsRequest::Status`].
    Status(StatusReport),
    /// Reply to [`CommsRequest::Rescan`]: the scan outcome.
    Rescanned {
        /// Files considered by the scan.
        scanned: usize,
        /// Files whose index entries were written or refreshed.
        updated: usize,
        /// Files pruned because they no longer exist.
        removed: usize,
        /// Wall-clock scan time in milliseconds.
        elapsed_ms: u64,
    },
    /// Reply to [`CommsRequest::ResolvedRefs`]: the resolved edges.
    ResolvedRefs(crate::comms::resolved_proto::ResolvedRefResult),
    /// Reply to [`CommsRequest::GitHistory`]: the sync outcome, the indexed HEAD, or a page of
    /// commits, per the op.
    GitHistory(crate::git_history::proto::GitHistoryReply),
    /// Reply to [`CommsRequest::Memory`]: the outcome of the forwarded memory operation.
    #[cfg(feature = "memory")]
    Memory(crate::comms::memory_proto::MemoryOutcome),
    /// Reply to [`CommsRequest::Governance`]: the outcome of the forwarded proposal operation.
    #[cfg(feature = "memory")]
    Governance(crate::comms::proposals_proto::GovernanceOutcome),
    /// Reply to [`CommsRequest::AccessedPaths`]: the daemon's currently-hot workspaces.
    Accessed {
        /// One row per hot workspace, most-recently-used first.
        workspaces: Vec<crate::comms::workspace_pool::AccessedWorkspace>,
    },
    /// Reply to [`CommsRequest::WorkspacesList`]: every registered workspace.
    Workspaces {
        /// The workspace rows, sorted by key.
        workspaces: Vec<crate::registry::WorkspaceRecord>,
    },
    /// Reply to [`CommsRequest::WorktreesList`]: the requested repo's worktrees.
    Worktrees {
        /// The worktree rows, sorted by name.
        worktrees: Vec<crate::registry::WorktreeRecord>,
    },
    /// Reply to [`CommsRequest::BranchesList`]: the requested repo's local branches.
    Branches {
        /// The branch rows, sorted by name.
        branches: Vec<crate::registry::BranchRecord>,
    },
    /// Reply to [`CommsRequest::WorktreeClaim`] / [`CommsRequest::WorktreeRelease`]: the outcome.
    ClaimOutcome {
        /// For a claim: `true` when the claim is now held by the claimant. For a release: `true`
        /// when a claim by the claimant was cleared. `false` otherwise (unknown or held by another).
        held: bool,
    },
    /// A request failed. `code` is a stable machine token; `message` is human detail.
    Error {
        /// Stable error token (e.g. `proto_skew`, `unknown_thread`, `not_creator`).
        code: String,
        /// Human-readable detail.
        message: String,
    },
}

/// A front-matter record paired with its per-thread `seq`. The `seq` is the position the message
/// occupies in its thread's append-only log; callers surface it so they can drive `inbox_ack`'s
/// `to_seq` bulk mode and `message_ids` resolution without an extra round-trip.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqMeta {
    /// The message's per-thread sequence number.
    pub seq: u64,
    /// The front-matter record. Flattened for a compact wire shape.
    #[serde(flatten)]
    pub meta: MessageMeta,
}

/// Daemon status snapshot returned by [`CommsRequest::Status`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// The daemon process id.
    pub pid: u32,
    /// Daemon build version.
    pub version: String,
    /// Protocol version spoken.
    pub proto_ver: u32,
    /// Seconds since the daemon started.
    pub uptime_secs: u64,
    /// Number of registered (active) threads.
    pub threads: u32,
    /// Number of live notification subscribers.
    pub subscribers: u32,
}

/// An unsolicited message the broker pushes to a subscribed link.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "notify", content = "data", rename_all = "snake_case")]
pub enum CommsNotification {
    /// A new message landed in a thread this link subscribes to. Carries front-matter only;
    /// fetch the body via [`CommsRequest::GetBody`].
    Message(MessageMeta),
    /// The daemon is shutting down; the link should disconnect.
    Shutdown,
}

/// A frame sent from broker → client: either a direct response to a request or an
/// out-of-band notification. Both ride the same link.
// `Eq` dropped because it wraps [`CommsResponse`], which is no longer `Eq` (its `Governance` outcome
// carries an `f32`-bearing `ProposalRecord`). `PartialEq` is retained for the round-trip tests.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommsOut {
    /// A reply to a specific request.
    Response(CommsResponse),
    /// An out-of-band push.
    Notification(CommsNotification),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_through_msgpack() {
        let req = CommsRequest::ThreadPost {
            thread: ThreadId::parse("th-1").expect("thread"),
            subject: "hi".to_string(),
            tags: vec!["t".to_string()],
            reply_to: None,
            body: b"hello".to_vec(),
        };
        let bytes = rmp_serde::to_vec_named(&req).expect("encode");
        let back: CommsRequest = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(req, back);
    }

    #[test]
    fn thread_start_round_trips() {
        let req = CommsRequest::ThreadStart {
            subject: Some("refactor".to_string()),
            path: Some("src/**".to_string()),
            members: vec![AgentId::parse("bob").expect("agent")],
        };
        let bytes = rmp_serde::to_vec_named(&req).expect("encode");
        let back: CommsRequest = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(req, back);
    }

    #[test]
    fn request_is_json_rpc_shaped() {
        let req = CommsRequest::Ping;
        let json = serde_json::to_value(&req).expect("json");
        assert_eq!(json["method"], "ping");
    }

    #[test]
    fn out_frame_round_trips() {
        let out = CommsOut::Notification(CommsNotification::Shutdown);
        let bytes = rmp_serde::to_vec_named(&out).expect("encode");
        let back: CommsOut = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(out, back);
    }

    fn sample_meta(id: &str) -> MessageMeta {
        MessageMeta {
            id: id.to_string(),
            thread: ThreadId::parse("th-1").expect("thread"),
            from: AgentId::parse("agent-1").expect("agent"),
            ts_micros: 7,
            subject: "subj".to_string(),
            tags: vec!["t".to_string()],
            reply_to: None,
            body_len: 3,
            body_sha: "abc".to_string(),
        }
    }

    #[test]
    fn seq_meta_round_trips_through_msgpack() {
        let value = SeqMeta {
            seq: 42,
            meta: sample_meta("m-1"),
        };
        let bytes = rmp_serde::to_vec_named(&value).expect("encode");
        let back: SeqMeta = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(value, back);
    }
}
