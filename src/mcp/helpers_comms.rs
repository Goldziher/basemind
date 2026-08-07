//! The `agents` domain dispatcher and the helper bodies for its sixteen modes.
//!
//! [`run_agents`] is the entry the `#[tool]` shim calls: it validates the flat [`AgentsParams`]
//! against the selected [`AgentsMode`] and delegates to the per-mode body. Each `run_<mode>` is a
//! thin proxy: acquire the lazily-connected [`CommsClient`](crate::comms::client::CommsClient) from
//! [`ServerState`], inject the server's resolved scope context (and identity, already baked into the
//! connected client), call the matching client method, and `json_result` the front-matter response.
//! Modes `history` and `inbox` surface front-matter ONLY — bodies are fetched exclusively through
//! mode `message`.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use tokio::sync::Mutex;

use super::ServerState;
use super::helpers::json_result;
use super::mode::{AgentsMode, reject_unsupported};
use super::types_comms::{
    AgentListParams, AgentListResponse, AgentRegisterParams, AgentRegisterResponse, AgentSummary, AgentsParams,
    CursorAdvance, InboxAckParams, InboxAckResponse, InboxReadParams, InboxReadResponse, InboxWaitParams,
    InboxWaitResponse, MessageFrontMatter, MessageGetParams, MessageGetResponse, ThreadArchiveParams,
    ThreadArchiveResponse, ThreadHistoryParams, ThreadHistoryResponse, ThreadJoinParams, ThreadLeaveParams,
    ThreadListParams, ThreadListResponse, ThreadMemberChangeResponse, ThreadMemberParams, ThreadMembersParams,
    ThreadMembersResponse, ThreadMembershipResponse, ThreadPostParams, ThreadPostResponse, ThreadStartParams,
    ThreadStartResponse, ThreadSummary,
};
use crate::comms::client::{CommsClient, scope_context_for};
use crate::comms::ids::AgentId;
use crate::comms::model::now_micros;

/// Default page size when a mode omits `limit`. Mirrors the broker's `DEFAULT_LIMIT`.
const DEFAULT_LIMIT: u32 = 100;

/// Default recency window for modes `history` / `inbox` when the caller omits `since_hours`.
const DEFAULT_SINCE_HOURS: u32 = 24;

/// Default long-poll timeout for mode `wait` when the caller omits `timeout_secs`.
const DEFAULT_WAIT_SECS: u32 = 30;

/// Hard cap on mode `wait`'s `timeout_secs`. Comfortably under the daemon's 30-minute idle-reap
/// window and short enough that one outstanding wait cannot meaningfully delay a drain.
const MAX_WAIT_SECS: u32 = 300;

/// Microseconds in one hour — the scale factor for the `since_hours` → `since_micros` cutoff.
const MICROS_PER_HOUR: i64 = 3_600_000_000;

/// Translate a caller-supplied `since_hours` window into the absolute `since_micros` cutoff. `None`
/// ⇒ the [`DEFAULT_SINCE_HOURS`] default; `Some(0)` ⇒ `None` (all history); otherwise `now - hours`.
fn since_cutoff(since_hours: Option<u32>) -> Option<i64> {
    let hours = since_hours.unwrap_or(DEFAULT_SINCE_HOURS);
    if hours == 0 {
        None
    } else {
        Some(now_micros() - i64::from(hours) * MICROS_PER_HOUR)
    }
}

/// Map a [`CommsClientError`](crate::comms::client::CommsClientError) into an MCP error with a
/// stable `comms:` prefix so agents can route on it.
pub(super) fn comms_err(error: impl std::fmt::Display) -> McpError {
    McpError::internal_error(format!("comms: {error}"), None)
}

/// Connect this MCP state to the comms broker without probing the daemon from a request it hosts.
async fn connect_comms_client(state: &ServerState, target: AgentId) -> Result<CommsClient, McpError> {
    let (remote, cwd) = scope_context_for(&state.shared.root);
    if state.shared.host.is_some() {
        let paths = crate::comms::singleton::resolve_paths().map_err(comms_err)?;
        return CommsClient::connect(&paths, target, remote, cwd)
            .await
            .map_err(comms_err);
    }
    CommsClient::ensure_and_connect(target, remote, cwd)
        .await
        .map_err(comms_err)
}

/// Validate the ≥2-of-3 addressing rule for mode `thread_start` client-side, so the caller gets a clear
/// error without a broker round-trip. The broker enforces the SAME rule; this is a fast pre-check.
/// The caller (creator) is always an implicit member, so `members` counts only when it names at
/// least one agent OTHER than the caller.
pub(super) fn validate_thread_dimensions(
    subject: Option<&str>,
    path: Option<&str>,
    members: &[AgentId],
    creator: &AgentId,
) -> Result<(), McpError> {
    let has_subject = subject.is_some_and(|s| !s.is_empty());
    let has_path = path.is_some_and(|p| !p.is_empty());
    let has_members = members.iter().any(|m| m != creator);
    let count = [has_subject, has_path, has_members].iter().filter(|b| **b).count();
    if count >= 2 {
        Ok(())
    } else {
        Err(comms_err(
            "`agents` mode=\"thread_start\" requires at least 2 of `subject` / `path` / `members` (a \
             member other than yourself); supply at least two",
        ))
    }
}

/// Resolve (lazily connecting + caching) the comms-broker client for the requested identity.
///
/// `as_agent` selects a sub-identity to act as; `None` resolves the server's own `agent_id`.
pub(super) async fn resolve_comms_client(
    state: &ServerState,
    as_agent: Option<String>,
) -> Result<Arc<Mutex<CommsClient>>, McpError> {
    let target = match as_agent {
        Some(raw) => AgentId::parse(raw.clone()).map_err(|e| comms_err(format!("invalid as_agent {raw:?}: {e}")))?,
        None => AgentId::parse(state.agent_id.clone())
            .map_err(|e| comms_err(format!("invalid agent id {:?}: {e}", state.agent_id)))?,
    };
    let mut map = state.comms_clients.lock().await;
    if let Some(handle) = map.get(&target) {
        return Ok(handle.clone());
    }
    let client = connect_comms_client(state, target.clone()).await?;
    let handle = Arc::new(Mutex::new(client));
    map.insert(target, handle.clone());
    Ok(handle)
}

/// Open a fresh, un-cached broker connection for the server's own identity. Long forwarded
/// operations (rescan / embed) use this instead of [`resolve_comms_client`] so they never hold the
/// shared per-identity client mutex that interactive comms tools + `resolved_refs` reads serialize
/// on — a multi-minute scan/embed would otherwise head-of-line-block every other comms call for
/// that identity (observed: a peer agent's message_get / thread_join / inbox_read all stalled for
/// minutes behind a forwarded embed pass). The returned client is owned and dropped by the caller.
pub(super) async fn connect_ephemeral_client(state: &ServerState) -> Result<CommsClient, McpError> {
    let target = AgentId::parse(state.agent_id.clone())
        .map_err(|e| comms_err(format!("invalid agent id {:?}: {e}", state.agent_id)))?;
    connect_comms_client(state, target).await
}

/// Route a CORE memory op to the machine's sole fjall writer and return the outcome.
///
/// On the daemon-HOSTED path ([`ServerState`]'s shared `host` is `Some`) the writer pool is
/// in-process, so the op runs directly against it on a blocking thread — no socket loopback, which on
/// the daemon would be the daemon dialing itself. Every other `daemon_writer` serve has no host and
/// forwards over the socket. Both run the identical `run_memory_op` writer-side, so callers see one
/// outcome shape. Centralizing the host-vs-forward choice here keeps each call site's op-construction
/// and outcome-match unchanged while the transport branch lives in exactly one place.
#[cfg(feature = "memory")]
pub(super) async fn dispatch_memory_op(
    state: &ServerState,
    op: crate::comms::memory_proto::MemoryOp,
) -> Result<crate::comms::memory_proto::MemoryOutcome, McpError> {
    if let Some(host) = &state.shared.host {
        let host = Arc::clone(host);
        let root = state.shared.root.clone();
        let scope = state.shared.scope.clone();
        return tokio::task::spawn_blocking(move || host.host_memory(&root, &scope, op))
            .await
            .map_err(|error| McpError::internal_error(format!("host memory task panicked: {error}"), None))?
            .map_err(|error| McpError::internal_error(format!("host memory: {error}"), None));
    }
    let client = resolve_comms_client(state, None).await?;
    let mut guard = client.lock().await;
    guard
        .memory_op(state.shared.root.clone(), state.shared.scope.clone(), op)
        .await
        .map_err(comms_err)
}

/// Route a PROPOSAL governance op to the machine's sole fjall writer and return the outcome. Same
/// host-vs-forward dispatch as [`dispatch_memory_op`], running `run_governance_op` writer-side.
#[cfg(feature = "memory")]
pub(super) async fn dispatch_governance_op(
    state: &ServerState,
    op: crate::comms::proposals_proto::GovernanceOp,
) -> Result<crate::comms::proposals_proto::GovernanceOutcome, McpError> {
    if let Some(host) = &state.shared.host {
        let host = Arc::clone(host);
        let root = state.shared.root.clone();
        let scope = state.shared.scope.clone();
        return tokio::task::spawn_blocking(move || host.host_governance(&root, &scope, op))
            .await
            .map_err(|error| McpError::internal_error(format!("host governance task panicked: {error}"), None))?
            .map_err(|error| McpError::internal_error(format!("host governance: {error}"), None));
    }
    let client = resolve_comms_client(state, None).await?;
    let mut guard = client.lock().await;
    guard
        .governance_op(state.shared.root.clone(), state.shared.scope.clone(), op)
        .await
        .map_err(comms_err)
}

/// Clamp a caller-supplied limit to `[1, MAX_LIMIT]`, defaulting when absent.
fn clamp_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, crate::comms::daemon::MAX_LIMIT)
}

/// Fail a mode that was given a field belonging to some other mode.
///
/// Inverted against `allowed` rather than listing every rejected field per mode: with sixteen modes
/// and twenty-three sibling fields, an explicit per-mode reject list is where a newly added field
/// silently becomes accept-everywhere.
fn reject_foreign_fields(mode: AgentsMode, present: &[(&str, bool)], allowed: &[&str]) -> Result<(), McpError> {
    let foreign: Vec<(&str, bool)> = present
        .iter()
        .filter(|(field, _)| !allowed.contains(field))
        .copied()
        .collect();
    reject_unsupported(AgentsMode::DOMAIN, mode.as_str(), &foreign)
}

/// Unwrap a field this mode cannot run without, naming the exact `mode`/field pair.
fn require_field<T>(mode: AgentsMode, field: &str, value: Option<T>) -> Result<T, McpError> {
    value.ok_or_else(|| {
        McpError::invalid_params(
            format!("`{}` mode=\"{}\" requires `{field}`", AgentsMode::DOMAIN, mode.as_str()),
            None,
        )
    })
}

/// Dispatch the single `agents` tool onto the per-mode body its `mode` selects.
///
/// Validation runs before the broker connection so a malformed call costs no daemon round-trip, and
/// fields belonging to another mode are rejected rather than dropped: a silently ignored `thread` on
/// an `inbox` call reads to an agent as a successful single-thread read.
pub(super) async fn run_agents(state: &ServerState, params: AgentsParams) -> Result<CallToolResult, McpError> {
    let AgentsParams {
        mode,
        thread,
        member,
        members,
        message_id,
        message_ids,
        to_seq,
        subject,
        subject_contains,
        path,
        body,
        tags,
        reply_to,
        include_archived,
        mark_read,
        cursor,
        limit,
        since_hours,
        timeout_secs,
        name,
        description,
        version,
        skills,
        as_agent,
    } = params;
    // `as_agent` is deliberately absent: it selects the identity the call runs as, not what the call
    // does, so every mode accepts it and no allow-list needs to repeat it. ~keep
    let present = [
        ("thread", thread.is_some()),
        ("member", member.is_some()),
        ("members", members.is_some()),
        ("message_id", message_id.is_some()),
        ("message_ids", message_ids.is_some()),
        ("to_seq", to_seq.is_some()),
        ("subject", subject.is_some()),
        ("subject_contains", subject_contains.is_some()),
        ("path", path.is_some()),
        ("body", body.is_some()),
        ("tags", tags.is_some()),
        ("reply_to", reply_to.is_some()),
        ("include_archived", include_archived.is_some()),
        ("mark_read", mark_read.is_some()),
        ("cursor", cursor.is_some()),
        ("limit", limit.is_some()),
        ("since_hours", since_hours.is_some()),
        ("timeout_secs", timeout_secs.is_some()),
        ("name", name.is_some()),
        ("description", description.is_some()),
        ("version", version.is_some()),
        ("skills", skills.is_some()),
    ];
    reject_foreign_fields(mode, &present, allowed_fields(mode))?;

    match mode {
        AgentsMode::Register => {
            run_agent_register(
                state,
                AgentRegisterParams {
                    name: name.unwrap_or_default(),
                    description: description.unwrap_or_default(),
                    version: version.unwrap_or_default(),
                    skills: skills.unwrap_or_default(),
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::List => run_agent_list(state, AgentListParams { thread, as_agent }).await,
        AgentsMode::ThreadStart => {
            run_thread_start(
                state,
                ThreadStartParams {
                    subject,
                    path,
                    members: members.unwrap_or_default(),
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::ThreadList => {
            run_thread_list(
                state,
                ThreadListParams {
                    subject_contains,
                    include_archived: include_archived.unwrap_or(false),
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Join => {
            run_thread_join(
                state,
                ThreadJoinParams {
                    thread: require_field(mode, "thread", thread)?,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Leave => {
            run_thread_leave(
                state,
                ThreadLeaveParams {
                    thread: require_field(mode, "thread", thread)?,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Members => {
            run_thread_members(
                state,
                ThreadMembersParams {
                    thread: require_field(mode, "thread", thread)?,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::AddMember => {
            run_thread_add_member(
                state,
                ThreadMemberParams {
                    thread: require_field(mode, "thread", thread)?,
                    member: require_field(mode, "member", member)?,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::RemoveMember => {
            run_thread_remove_member(
                state,
                ThreadMemberParams {
                    thread: require_field(mode, "thread", thread)?,
                    member: require_field(mode, "member", member)?,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Archive => {
            run_thread_archive(
                state,
                ThreadArchiveParams {
                    thread: require_field(mode, "thread", thread)?,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Post => {
            run_thread_post(
                state,
                ThreadPostParams {
                    thread: require_field(mode, "thread", thread)?,
                    subject: require_field(mode, "subject", subject)?,
                    body,
                    tags,
                    reply_to,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::History => {
            run_thread_history(
                state,
                ThreadHistoryParams {
                    thread: require_field(mode, "thread", thread)?,
                    cursor,
                    limit,
                    since_hours,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Message => {
            run_message_get(
                state,
                MessageGetParams {
                    message_id: require_field(mode, "message_id", message_id)?,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Inbox => {
            run_inbox_read(
                state,
                InboxReadParams {
                    cursor,
                    limit,
                    mark_read: mark_read.unwrap_or(false),
                    since_hours,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Ack => {
            run_inbox_ack(
                state,
                InboxAckParams {
                    message_ids: message_ids.unwrap_or_default(),
                    thread,
                    to_seq,
                    as_agent,
                },
            )
            .await
        }
        AgentsMode::Wait => {
            run_inbox_wait(
                state,
                InboxWaitParams {
                    timeout_secs,
                    thread,
                    since_hours,
                    cursor,
                    as_agent,
                },
            )
            .await
        }
    }
}

/// The sibling fields each mode accepts. Everything else present on the call is rejected by
/// [`reject_foreign_fields`], so a parameter an agent believed took effect never silently doesn't.
fn allowed_fields(mode: AgentsMode) -> &'static [&'static str] {
    match mode {
        AgentsMode::Register => &["name", "description", "version", "skills"],
        AgentsMode::List => &["thread"],
        AgentsMode::ThreadStart => &["subject", "path", "members"],
        AgentsMode::ThreadList => &["subject_contains", "include_archived"],
        AgentsMode::Join | AgentsMode::Leave | AgentsMode::Members | AgentsMode::Archive => &["thread"],
        AgentsMode::AddMember | AgentsMode::RemoveMember => &["thread", "member"],
        AgentsMode::Post => &["thread", "subject", "body", "tags", "reply_to"],
        AgentsMode::History => &["thread", "cursor", "limit", "since_hours"],
        AgentsMode::Message => &["message_id"],
        AgentsMode::Inbox => &["cursor", "limit", "mark_read", "since_hours"],
        AgentsMode::Ack => &["message_ids", "thread", "to_seq"],
        AgentsMode::Wait => &["thread", "timeout_secs", "since_hours", "cursor"],
    }
}

async fn run_agent_register(state: &ServerState, params: AgentRegisterParams) -> Result<CallToolResult, McpError> {
    let card = crate::comms::model::AgentCard {
        name: params.name,
        description: params.description,
        version: params.version,
        skills: params.skills,
    };
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let agent_id = client.agent().as_str().to_string();
    client.register_agent(card).await.map_err(comms_err)?;
    json_result(&AgentRegisterResponse {
        agent_id,
        registered: true,
    })
}

async fn run_agent_list(state: &ServerState, params: AgentListParams) -> Result<CallToolResult, McpError> {
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let records = client.list_agents(params.thread).await.map_err(comms_err)?;
    let agents: Vec<AgentSummary> = records
        .iter()
        .map(|r| AgentSummary {
            agent_id: r.agent_id.as_str().to_string(),
            name: r.card.name.clone(),
            description: r.card.description.clone(),
            version: r.card.version.clone(),
            skills: r.card.skills.clone(),
            first_seen: r.first_seen,
            last_seen: r.last_seen,
        })
        .collect();
    json_result(&AgentListResponse {
        total: agents.len(),
        agents,
    })
}

async fn run_thread_start(state: &ServerState, params: ThreadStartParams) -> Result<CallToolResult, McpError> {
    let creator = match &params.as_agent {
        Some(raw) => AgentId::parse(raw.clone()).map_err(|e| comms_err(format!("invalid as_agent {raw:?}: {e}")))?,
        None => AgentId::parse(state.agent_id.clone())
            .map_err(|e| comms_err(format!("invalid agent id {:?}: {e}", state.agent_id)))?,
    };
    validate_thread_dimensions(
        params.subject.as_deref(),
        params.path.as_deref(),
        &params.members,
        &creator,
    )?;
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let thread = client
        .start_thread(params.subject, params.path, params.members)
        .await
        .map_err(comms_err)?;
    json_result(&ThreadStartResponse {
        thread: ThreadSummary::from_thread(&thread, now_micros()),
    })
}

async fn run_thread_list(state: &ServerState, params: ThreadListParams) -> Result<CallToolResult, McpError> {
    let (remote, cwd) = scope_context_for(&state.shared.root);
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let threads = client
        .list_threads(remote, cwd, params.subject_contains, params.include_archived)
        .await
        .map_err(comms_err)?;
    let now = now_micros();
    let summaries: Vec<ThreadSummary> = threads.iter().map(|t| ThreadSummary::from_thread(t, now)).collect();
    json_result(&ThreadListResponse {
        total: summaries.len(),
        threads: summaries,
    })
}

async fn run_thread_join(state: &ServerState, params: ThreadJoinParams) -> Result<CallToolResult, McpError> {
    let label = params.thread.as_str().to_string();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    client.join_thread(params.thread).await.map_err(comms_err)?;
    json_result(&ThreadMembershipResponse {
        thread: label,
        joined: true,
        left: false,
    })
}

async fn run_thread_leave(state: &ServerState, params: ThreadLeaveParams) -> Result<CallToolResult, McpError> {
    let label = params.thread.as_str().to_string();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    client.leave_thread(params.thread).await.map_err(comms_err)?;
    json_result(&ThreadMembershipResponse {
        thread: label,
        joined: false,
        left: true,
    })
}

async fn run_thread_members(state: &ServerState, params: ThreadMembersParams) -> Result<CallToolResult, McpError> {
    let label = params.thread.as_str().to_string();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let members = client.thread_members(params.thread).await.map_err(comms_err)?;
    json_result(&ThreadMembersResponse {
        thread: label,
        members: members.iter().map(|m| m.as_str().to_string()).collect(),
    })
}

async fn run_thread_add_member(state: &ServerState, params: ThreadMemberParams) -> Result<CallToolResult, McpError> {
    let thread = params.thread.as_str().to_string();
    let member = params.member.as_str().to_string();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    client
        .add_member(params.thread, params.member)
        .await
        .map_err(comms_err)?;
    json_result(&ThreadMemberChangeResponse {
        thread,
        member,
        added: true,
        removed: false,
    })
}

async fn run_thread_remove_member(state: &ServerState, params: ThreadMemberParams) -> Result<CallToolResult, McpError> {
    let thread = params.thread.as_str().to_string();
    let member = params.member.as_str().to_string();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    client
        .remove_member(params.thread, params.member)
        .await
        .map_err(comms_err)?;
    json_result(&ThreadMemberChangeResponse {
        thread,
        member,
        added: false,
        removed: true,
    })
}

async fn run_thread_archive(state: &ServerState, params: ThreadArchiveParams) -> Result<CallToolResult, McpError> {
    let label = params.thread.as_str().to_string();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    client.archive_thread(params.thread).await.map_err(comms_err)?;
    json_result(&ThreadArchiveResponse {
        thread: label,
        archived: true,
    })
}

async fn run_thread_post(state: &ServerState, params: ThreadPostParams) -> Result<CallToolResult, McpError> {
    let body = params.body.unwrap_or_default().into_bytes();
    let tags = params.tags.unwrap_or_default();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let message_id = client
        .post_message(params.thread, params.subject, body, tags, params.reply_to)
        .await
        .map_err(comms_err)?;
    json_result(&ThreadPostResponse { message_id })
}

async fn run_thread_history(state: &ServerState, params: ThreadHistoryParams) -> Result<CallToolResult, McpError> {
    let limit = clamp_limit(params.limit);
    let cursor = params.cursor.map(crate::comms::cursor::Cursor);
    let since = since_cutoff(params.since_hours);
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let (metas, next_cursor) = client
        .read_history(params.thread, cursor, limit, since)
        .await
        .map_err(comms_err)?;
    let now = now_micros();
    let messages: Vec<MessageFrontMatter> = metas
        .iter()
        .map(|sm| MessageFrontMatter::from_seq_meta(sm, now))
        .collect();
    json_result(&ThreadHistoryResponse {
        total: messages.len(),
        messages,
        next_cursor,
    })
}

async fn run_message_get(state: &ServerState, params: MessageGetParams) -> Result<CallToolResult, McpError> {
    let message_id = params.message_id.clone();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let body = client.get_body(params.message_id).await.map_err(comms_err)?;
    let found = body.is_some();
    let body = body.map(|b| String::from_utf8_lossy(&b).into_owned());
    json_result(&MessageGetResponse {
        message_id,
        found,
        body,
    })
}

async fn run_inbox_read(state: &ServerState, params: InboxReadParams) -> Result<CallToolResult, McpError> {
    let limit = clamp_limit(params.limit);
    let cursor = params.cursor.map(crate::comms::cursor::Cursor);
    let since = since_cutoff(params.since_hours);
    let (remote, cwd) = scope_context_for(&state.shared.root);
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let (metas, unread, next_cursor) = client
        .read_inbox(remote, cwd, cursor, limit, params.mark_read, since)
        .await
        .map_err(comms_err)?;
    let now = now_micros();
    let messages: Vec<MessageFrontMatter> = metas
        .iter()
        .map(|sm| MessageFrontMatter::from_seq_meta(sm, now))
        .collect();
    json_result(&InboxReadResponse {
        total: messages.len(),
        unread,
        messages,
        next_cursor,
    })
}

async fn run_inbox_ack(state: &ServerState, params: InboxAckParams) -> Result<CallToolResult, McpError> {
    let has_bulk = params.thread.is_some() && params.to_seq.is_some();
    if params.message_ids.is_empty() && !has_bulk {
        return Err(comms_err(
            "`agents` mode=\"ack\" requires `message_ids`, or a (`thread`, `to_seq`) pair",
        ));
    }
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let (acked, cursors) = client
        .ack_inbox(params.message_ids, params.thread, params.to_seq)
        .await
        .map_err(comms_err)?;
    let cursors_advanced: Vec<CursorAdvance> = cursors
        .into_iter()
        .map(|(thread, seq)| CursorAdvance { thread, seq })
        .collect();
    json_result(&InboxAckResponse {
        acked: acked as usize,
        cursors_advanced,
    })
}

/// Long-poll the inbox and return as soon as a peer posts (or on timeout).
///
/// LOAD-BEARING: this opens its OWN ephemeral [`CommsClient`], never the shared cached
/// `Arc<Mutex<CommsClient>>` behind [`resolve_comms_client`]. Locking that shared client for the
/// wait would hold its mutex for up to `timeout_secs`, head-of-line-blocking every OTHER comms
/// tool call for this identity (agent_list, thread_post, inbox_read, …) for the whole wait. A
/// fresh connection per wait avoids that at the cost of one extra link + broker sink per
/// outstanding call — an accepted trade-off (see the design brief's risk notes).
async fn run_inbox_wait(state: &ServerState, params: InboxWaitParams) -> Result<CallToolResult, McpError> {
    let timeout_secs = params.timeout_secs.unwrap_or(DEFAULT_WAIT_SECS).clamp(1, MAX_WAIT_SECS);
    let cursor = params.cursor.map(crate::comms::cursor::Cursor);
    let since = since_cutoff(params.since_hours);
    let (remote, cwd) = scope_context_for(&state.shared.root);

    let agent = match &params.as_agent {
        Some(raw) => AgentId::parse(raw.clone()).map_err(|e| comms_err(format!("invalid as_agent {raw:?}: {e}")))?,
        None => AgentId::parse(state.agent_id.clone())
            .map_err(|e| comms_err(format!("invalid agent id {:?}: {e}", state.agent_id)))?,
    };
    let mut client = connect_comms_client(state, agent).await?;

    let (timed_out, metas, unread, next_cursor) = client
        .wait_inbox(
            remote,
            cwd,
            params.thread,
            since,
            cursor,
            DEFAULT_LIMIT,
            std::time::Duration::from_secs(u64::from(timeout_secs)),
        )
        .await
        .map_err(comms_err)?;

    let now = now_micros();
    let messages: Vec<MessageFrontMatter> = metas
        .iter()
        .map(|sm| MessageFrontMatter::from_seq_meta(sm, now))
        .collect();
    json_result(&InboxWaitResponse {
        timed_out,
        total: messages.len(),
        unread,
        messages,
        next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_name_the_mode_and_field_when_a_required_sibling_is_missing() {
        let error = require_field(AgentsMode::Post, "subject", None::<String>).expect_err("a bodyless post fails");
        assert_eq!(error.message.to_string(), "`agents` mode=\"post\" requires `subject`");
    }

    #[test]
    fn should_reject_a_field_that_belongs_to_another_mode() {
        let error = reject_foreign_fields(
            AgentsMode::Inbox,
            &[("thread", true), ("limit", true)],
            allowed_fields(AgentsMode::Inbox),
        )
        .expect_err("`thread` is an `ack`/`wait` field, not an `inbox` one");
        let message = error.message.to_string();
        assert_eq!(message, "`agents` mode `inbox` does not accept `thread`");
    }

    #[test]
    fn should_allow_as_agent_on_every_mode_without_repeating_it_per_allow_list() {
        for mode in AgentsMode::ALL {
            assert!(
                !allowed_fields(*mode).contains(&"as_agent"),
                "`as_agent` is universal and must not be listed per mode ({mode})"
            );
        }
    }
}
