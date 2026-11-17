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

use super::agents_hub::{AgentCleanupReport, AgentStoreStatus};
use super::v2::api::{
    CommsError, InboxBody, InboxReply, PostBody, PostReply, Principal, SubName, ThreadId, ThreadQuery, ThreadUpdate,
    ThreadsReply,
};

/// The protocol version this build speaks. Bumped on any breaking change to the request /
/// response / notification shapes. Negotiated in [`CommsRequest::Hello`], so a skewed peer fails
/// there with a clear error instead of misreading a frame mid-session. 1→2 the room→thread
/// redesign; 2→3 inbox subscriptions; 3→4 request correlation ([`CommsRequest::Call`] /
/// [`CommsOut::Reply`]); 4→5 the `governance` requests removed; 5→6 the batched memory reads
/// and `Rescan`'s `verify` flag. 6→7 agent comms v2 (ADR-0021, ADR-0030 rule 13): `Hello` names a
/// principal and a root, and the four agent requests replace the twenty of the v1 broker.
///
/// A request no client of this build sends is retired without a bump, so an older binary of the
/// same generation keeps using a newer daemon for everything else (ADR-0024 rule 2): the worktree
/// claims, and the forwarded `memory`, `resolved_refs`, `index_read` and `code_search_lanes` reads
/// once a CLI next to the daemon forwarded its whole call instead (ADR-0039). An older peer that
/// still sends one has that link closed by the decode error.
pub const PROTO_VER: u32 = 7;

/// A request from a client to the broker. `method` selects the variant; `params` are the
/// flattened fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum CommsRequest {
    /// First frame on a link: negotiate the protocol version and fix who the link speaks for.
    ///
    /// `principal` and `root` are what every agent request on the link acts as and in: the
    /// principal the client resolved (ADR-0021), and the canonical root its repository is derived
    /// from. A link that only manages the daemon or forwards index work sends neither, and its
    /// agent requests are refused. `host` is the agent harness (`BASEMIND_HOST`), for status only.
    Hello {
        /// The protocol version the client speaks.
        proto_ver: u32,
        /// The identity the link's agent requests act as.
        #[serde(default)]
        principal: Option<Principal>,
        /// The canonical checkout root the identity was derived from.
        #[serde(default)]
        root: Option<std::path::PathBuf>,
        /// The agent harness, from `BASEMIND_HOST`.
        #[serde(default)]
        host: Option<String>,
    },
    /// `agent_post`.
    AgentPost {
        /// The sub-identity (`as`) the call acts as, under the link's principal.
        #[serde(default)]
        sub: Option<SubName>,
        /// The post.
        body: PostBody,
    },
    /// `agent_inbox`. The daemon owns the wait: with `wait_secs` above 0 a read that finds
    /// nothing unread waits for a post, up to that many seconds.
    AgentInbox {
        /// The sub-identity (`as`) the call acts as, under the link's principal.
        #[serde(default)]
        sub: Option<SubName>,
        /// The read.
        body: InboxBody,
        /// Seconds to wait while nothing is unread, at most `MAX_WAIT_SECS`.
        #[serde(default)]
        wait_secs: u32,
    },
    /// `agent_threads` without a change: list, discover or describe.
    AgentThreads {
        /// The sub-identity (`as`) the call acts as, under the link's principal.
        #[serde(default)]
        sub: Option<SubName>,
        /// What to list or describe.
        query: ThreadQuery,
    },
    /// `agent_threads` with a change: join, leave, invite or archive.
    AgentThreadUpdate {
        /// The sub-identity (`as`) the call acts as, under the link's principal.
        #[serde(default)]
        sub: Option<SubName>,
        /// The change.
        update: ThreadUpdate,
    },
    /// `basemind agent status`: the agent store's state, and the link's repository.
    AgentStatus,
    /// `basemind agent cleanup`: archive these threads whoever created them, then sweep once.
    AgentCleanup {
        /// Threads to archive.
        #[serde(default)]
        archive: Vec<ThreadId>,
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
        /// The paths were named by a person or an agent (`basemind rescan <paths>`, `index_rescan
        /// paths`), not seen changing by a watcher: a named file that reads as unchanged has its
        /// blob validated instead of trusted for existing. Ignored by a full scan.
        #[serde(default)]
        verify: bool,
    },
    /// Forward a git-history operation to the daemon. `git-history.fjall/` is a fjall database, and
    /// fjall's directory lock is exclusive — so exactly ONE process may hold a repo's history index.
    /// Under this model that process is the daemon: it BUILDS the index ([`GitHistoryOp::Sync`],
    /// serialized per repo so N sessions produce one build) and answers the front-ends' history
    /// reads from it. No other process opens the database while the daemon is up — building it
    /// elsewhere would both steal the daemon's lock and run a multi-GB, minutes-long history walk
    /// inside the process an agent is actively querying.
    ///
    /// [`GitHistoryOp`]: crate::git_history::proto::GitHistoryOp
    GitHistory {
        /// Canonical workspace root (worktree root), selecting the repo's index.
        root: std::path::PathBuf,
        /// The operation to run.
        op: crate::git_history::proto::GitHistoryOp,
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
    /// Liveness probe. The daemon replies [`CommsResponse::Pong`].
    Ping,
    /// Ask the daemon to drain and stop. Used by `basemind comms stop`.
    Stop,
    /// Report daemon status (pid / version / uptime / thread + subscriber counts).
    Status,
    /// A correlated request: the broker handles `request` and answers with a [`CommsOut::Reply`]
    /// echoing `id`, so a client that abandoned an earlier request (a cancelled future) can tell
    /// the stale reply from the one it is waiting for. A bare request is still answered with an
    /// uncorrelated [`CommsOut::Response`]. `Hello` is always sent bare (it negotiates the
    /// protocol version, so it must stay decodable by a peer that speaks a different one).
    Call {
        /// Client-chosen id, unique per link and echoed in the reply.
        id: u64,
        /// The wrapped request.
        request: Box<CommsRequest>,
    },
}

impl CommsRequest {
    /// The wire `method` spelling of this variant, matching the serde `rename_all = "snake_case"`
    /// representation. Borrowing rather than serializing keeps it free at the call site, which
    /// matters because the daemon names the failing request when it records a terminal store
    /// failure — the one piece of evidence a poisoned daemon otherwise takes to the grave.
    pub fn method(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::AgentPost { .. } => "agent_post",
            Self::AgentInbox { .. } => "agent_inbox",
            Self::AgentThreads { .. } => "agent_threads",
            Self::AgentThreadUpdate { .. } => "agent_thread_update",
            Self::AgentStatus => "agent_status",
            Self::AgentCleanup { .. } => "agent_cleanup",
            Self::Rescan { .. } => "rescan",
            Self::GitHistory { .. } => "git_history",
            Self::AccessedPaths { .. } => "accessed_paths",
            Self::WorkspacesList { .. } => "workspaces_list",
            Self::WorktreesList { .. } => "worktrees_list",
            Self::BranchesList { .. } => "branches_list",
            Self::Ping => "ping",
            Self::Stop => "stop",
            Self::Status => "status",
            Self::Call { request, .. } => request.method(),
        }
    }
}

/// A response from the broker to a [`CommsRequest`].
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
    /// Reply to [`CommsRequest::AgentPost`].
    AgentPosted(PostReply),
    /// Reply to [`CommsRequest::AgentInbox`].
    AgentRead(InboxReply),
    /// Reply to [`CommsRequest::AgentThreads`] and [`CommsRequest::AgentThreadUpdate`].
    AgentThreads(ThreadsReply),
    /// Reply to [`CommsRequest::AgentStatus`].
    AgentStatus(AgentStoreStatus),
    /// Reply to [`CommsRequest::AgentCleanup`].
    AgentCleaned(AgentCleanupReport),
    /// An agent request refused, with its stable code, its fields and the message to show.
    AgentRefused(CommsError),
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
        /// Documents re-extracted and re-embedded. Counted apart from `updated`, which is code-map
        /// only: a document has no tree-sitter grammar, so without this a document-only rescan
        /// reports `updated: 0` and reads as though nothing was refreshed.
        ///
        /// Additive with `#[serde(default)]` so a daemon and a client on either side of this change
        /// still talk: an older peer simply omits the field and it decodes as 0, which is what it
        /// would have reported anyway.
        #[serde(default)]
        docs_indexed: usize,
        /// Documents chunked by the linear fallback (over the `[documents]` chunk cutovers). Additive
        /// with `#[serde(default)]`, like `docs_indexed`: an older peer omits it and it decodes as 0.
        #[serde(default)]
        docs_degraded: usize,
        /// Subset of `docs_degraded` re-read as `text/plain` after a structured-extraction failure.
        /// Additive with `#[serde(default)]` like its siblings.
        #[serde(default)]
        docs_text_fallback: usize,
        /// Documents abandoned because extraction overran `[documents] extraction_timeout_secs`.
        /// Additive with `#[serde(default)]`; an older peer omits it and it decodes as 0.
        #[serde(default)]
        doc_timeouts: usize,
        /// Files pruned because they no longer exist.
        removed: usize,
        /// Wall-clock scan time in milliseconds.
        elapsed_ms: u64,
    },
    /// Reply to [`CommsRequest::GitHistory`]: the sync outcome, the indexed HEAD, or a page of
    /// commits, per the op.
    GitHistory(crate::git_history::proto::GitHistoryReply),
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
    /// A request failed. `code` is a stable machine token; `message` is human detail.
    Error {
        /// Stable error token (e.g. `proto_skew`, `unknown_thread`, `not_creator`).
        code: String,
        /// Human-readable detail.
        message: String,
    },
}

/// Daemon status snapshot returned by [`CommsRequest::Status`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// The daemon process id.
    pub pid: u32,
    /// Daemon build version.
    pub version: String,
    /// Short hash of the daemon's own executable. Distinguishes builds that share a `version`, so a
    /// daemon left over from a previous local install can be told apart from the current binary
    /// instead of silently passing every version check. Additive with `#[serde(default)]`: an older
    /// daemon omits it and it decodes empty, which reads as "this daemon cannot report its build".
    #[serde(default)]
    pub build_id: String,
    /// Protocol version spoken.
    pub proto_ver: u32,
    /// Seconds since the daemon started.
    pub uptime_secs: u64,
    /// State of the agent store: `open`, `newer` (written by a newer basemind; read-only) or
    /// `unreadable`.
    pub agent_store: String,
}

/// An unsolicited message the broker pushes to a link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "notify", content = "data", rename_all = "snake_case")]
pub enum CommsNotification {
    /// The daemon is shutting down; the link should disconnect.
    Shutdown,
}

/// A frame sent from broker → client: either a direct response to a request or an
/// out-of-band notification. Both ride the same link.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommsOut {
    /// An uncorrelated reply to a bare request.
    Response(CommsResponse),
    /// The reply to a [`CommsRequest::Call`], carrying the request's id.
    Reply {
        /// The id of the `Call` this answers.
        id: u64,
        /// The response.
        response: CommsResponse,
    },
    /// An out-of-band push.
    Notification(CommsNotification),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A binary older than whole-call forwarding (ADR-0039) can still send a forwarded read. The
    /// frame fails to decode, so that link is dropped, and nothing else of the protocol changed.
    #[test]
    fn a_retired_forwarded_read_is_a_decode_error() {
        #[derive(Serialize)]
        struct Frame {
            method: &'static str,
            params: serde_json::Value,
        }
        for method in ["memory", "resolved_refs", "index_read", "code_search_lanes"] {
            let bytes = rmp_serde::to_vec_named(&Frame {
                method,
                params: serde_json::json!({ "root": "/repo", "scope": "path:/repo", "op": {}, "query": {} }),
            })
            .expect("encode");
            let error = rmp_serde::from_slice::<CommsRequest>(&bytes).expect_err("a retired method must not decode");
            assert!(
                error.to_string().contains(method),
                "the error names the method: {error}"
            );
        }
        let rescan = rmp_serde::to_vec_named(&Frame {
            method: "rescan",
            params: serde_json::json!({ "root": "/repo" }),
        })
        .expect("encode");
        assert!(matches!(
            rmp_serde::from_slice::<CommsRequest>(&rescan).expect("a live method still decodes"),
            CommsRequest::Rescan { .. }
        ));
    }

    /// A client older than the removal of worktree claims can still send one. The frame must fail
    /// to decode as an error the link loop turns into a dropped connection — never a panic, and
    /// never a silent match against some other variant.
    #[test]
    fn a_retired_worktree_claim_request_is_a_decode_error() {
        #[derive(Serialize)]
        struct Params {
            repo_id: &'static str,
            name: &'static str,
            claimant: &'static str,
        }
        #[derive(Serialize)]
        struct Frame {
            method: &'static str,
            params: Params,
        }
        for method in ["worktree_claim", "worktree_release"] {
            let bytes = rmp_serde::to_vec_named(&Frame {
                method,
                params: Params {
                    repo_id: "path:/repo",
                    name: "(main)",
                    claimant: "agent-a",
                },
            })
            .expect("encode");
            let error = rmp_serde::from_slice::<CommsRequest>(&bytes).expect_err("a retired method must not decode");
            assert!(
                error.to_string().contains(method),
                "the error names the method: {error}"
            );
        }
        let live = rmp_serde::to_vec_named(&Frame {
            method: "worktrees_list",
            params: Params {
                repo_id: "path:/repo",
                name: "(main)",
                claimant: "agent-a",
            },
        })
        .expect("encode");
        assert_eq!(
            rmp_serde::from_slice::<CommsRequest>(&live).expect("a live method decodes from the same frame shape"),
            CommsRequest::WorktreesList {
                repo_id: "path:/repo".to_string()
            }
        );
    }

    fn thread() -> ThreadId {
        ThreadId::parse("t-aaaaaa").expect("thread")
    }

    fn round_trip(req: &CommsRequest) -> CommsRequest {
        let bytes = rmp_serde::to_vec_named(req).expect("encode");
        rmp_serde::from_slice(&bytes).expect("decode")
    }

    #[test]
    fn agent_requests_round_trip_through_msgpack() {
        let requests = [
            CommsRequest::Hello {
                proto_ver: PROTO_VER,
                principal: Some(Principal::parse("agent-repo-abcdef").expect("principal")),
                root: Some(std::path::PathBuf::from("/repo")),
                host: Some("claude".to_string()),
            },
            CommsRequest::AgentPost {
                sub: Some(SubName::parse("worker").expect("sub")),
                body: PostBody {
                    target: crate::comms::v2::api::PostTarget::Thread(thread()),
                    body: "hello".to_string(),
                },
            },
            CommsRequest::AgentInbox {
                sub: None,
                body: InboxBody::default(),
                wait_secs: 30,
            },
            CommsRequest::AgentThreads {
                sub: None,
                query: ThreadQuery::Describe(thread()),
            },
            CommsRequest::AgentThreadUpdate {
                sub: None,
                update: ThreadUpdate {
                    thread: thread(),
                    action: crate::comms::v2::api::ThreadAction::Join,
                },
            },
            CommsRequest::AgentStatus,
            CommsRequest::AgentCleanup {
                archive: vec![thread()],
            },
        ];
        for req in requests {
            assert_eq!(round_trip(&req), req);
        }
    }

    #[test]
    fn a_refusal_crosses_the_wire_with_its_code() {
        let refused = CommsResponse::AgentRefused(CommsError::ThreadNotFound { thread: thread() });
        let bytes = rmp_serde::to_vec_named(&refused).expect("encode");
        assert_eq!(rmp_serde::from_slice::<CommsResponse>(&bytes).expect("decode"), refused);
    }

    /// A v1 agent request from a client that skipped the `Hello` check does not decode, so the link
    /// drops rather than misreading it.
    #[test]
    fn a_v1_agent_request_is_a_decode_error() {
        for method in ["thread_post", "inbox", "ack_inbox", "subscribe_inbox", "register"] {
            let frame = serde_json::json!({ "method": method, "params": {} });
            let bytes = rmp_serde::to_vec_named(&frame).expect("encode");
            assert!(
                rmp_serde::from_slice::<CommsRequest>(&bytes).is_err(),
                "{method} decoded"
            );
        }
    }

    #[test]
    fn rescanned_without_doc_guard_counters_decodes_as_zero() {
        // An older daemon omits `docs_degraded` / `doc_timeouts`; a newer client must read them as 0.
        let old = serde_json::json!({
            "result": "rescanned",
            "data": {"scanned": 3, "updated": 2, "docs_indexed": 1, "removed": 0, "elapsed_ms": 7}
        });
        let bytes = rmp_serde::to_vec_named(&old).expect("encode");
        let back: CommsResponse = rmp_serde::from_slice(&bytes).expect("decode");
        assert!(matches!(
            back,
            CommsResponse::Rescanned {
                docs_indexed: 1,
                docs_degraded: 0,
                docs_text_fallback: 0,
                doc_timeouts: 0,
                ..
            }
        ));
        let new = CommsResponse::Rescanned {
            scanned: 3,
            updated: 2,
            docs_indexed: 1,
            docs_degraded: 4,
            docs_text_fallback: 2,
            doc_timeouts: 5,
            removed: 0,
            elapsed_ms: 7,
        };
        let bytes = rmp_serde::to_vec_named(&new).expect("encode");
        assert_eq!(rmp_serde::from_slice::<CommsResponse>(&bytes).expect("decode"), new);
    }

    #[test]
    fn method_matches_the_serde_tag() {
        // `method()` is a hand-written match, so it can drift from the derived `rename_all`
        // spelling silently. Check it against what serde actually emits for a sample spanning
        // unit variants, struct variants, and the multi-word names most at risk. ~keep
        let samples = [
            CommsRequest::Ping,
            CommsRequest::Stop,
            CommsRequest::Status,
            CommsRequest::AgentStatus,
            CommsRequest::AgentCleanup { archive: Vec::new() },
            CommsRequest::AgentInbox {
                sub: None,
                body: InboxBody::default(),
                wait_secs: 0,
            },
            CommsRequest::AgentThreadUpdate {
                sub: None,
                update: ThreadUpdate {
                    thread: thread(),
                    action: crate::comms::v2::api::ThreadAction::Leave,
                },
            },
        ];
        for req in samples {
            let value = serde_json::to_value(&req).expect("serialize");
            let tag = value.get("method").and_then(serde_json::Value::as_str).expect("tagged");
            assert_eq!(req.method(), tag, "method() drifted from the serde tag for {req:?}");
        }
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
}
