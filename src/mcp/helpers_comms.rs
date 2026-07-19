//! Helper bodies for the agent-comms MCP tools.
//!
//! Each `run_<tool>` is a thin proxy: acquire the lazily-connected
//! [`CommsClient`](crate::comms::client::CommsClient) from [`ServerState`], inject the server's
//! resolved scope context (and identity, already baked into the connected client), call the
//! matching client method, and `json_result` the front-matter response. History and inbox
//! tools surface front-matter ONLY — bodies are fetched exclusively through `message_get`.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use tokio::sync::Mutex;

use super::ServerState;
use super::helpers::json_result;
use super::types_comms::{
    AgentListParams, AgentListResponse, AgentRegisterParams, AgentRegisterResponse, AgentSummary, CursorAdvance,
    InboxAckParams, InboxAckResponse, InboxReadParams, InboxReadResponse, InboxWaitParams, InboxWaitResponse,
    MessageFrontMatter, MessageGetParams, MessageGetResponse, ThreadArchiveParams, ThreadArchiveResponse,
    ThreadHistoryParams, ThreadHistoryResponse, ThreadJoinParams, ThreadLeaveParams, ThreadListParams,
    ThreadListResponse, ThreadMemberChangeResponse, ThreadMemberParams, ThreadMembersParams, ThreadMembersResponse,
    ThreadMembershipResponse, ThreadPostParams, ThreadPostResponse, ThreadStartParams, ThreadStartResponse,
    ThreadSummary,
};
use crate::comms::client::{CommsClient, scope_context_for};
use crate::comms::ids::AgentId;
use crate::comms::model::now_micros;

/// Default page size when a comms tool omits `limit`. Mirrors the broker's `DEFAULT_LIMIT`.
const DEFAULT_LIMIT: u32 = 100;

/// Default recency window for `thread_history` / `inbox_read` when the caller omits `since_hours`.
const DEFAULT_SINCE_HOURS: u32 = 24;

/// Default long-poll timeout for `inbox_wait` when the caller omits `timeout_secs`.
const DEFAULT_WAIT_SECS: u32 = 30;

/// Hard cap on `inbox_wait`'s `timeout_secs`. Comfortably under the daemon's 30-minute idle-reap
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

/// Validate the ≥2-of-3 addressing rule for `thread_start` client-side, so the caller gets a clear
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
            "thread_start requires at least 2 of subject / path / members (a member other than \
             yourself); supply at least two",
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
    let (remote, cwd) = scope_context_for(&state.root);
    let client = CommsClient::ensure_and_connect(target.clone(), remote, cwd)
        .await
        .map_err(comms_err)?;
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
    let (remote, cwd) = scope_context_for(&state.root);
    CommsClient::ensure_and_connect(target, remote, cwd).await.map_err(comms_err)
}

/// Clamp a caller-supplied limit to `[1, MAX_LIMIT]`, defaulting when absent.
fn clamp_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, crate::comms::daemon::MAX_LIMIT)
}

pub(super) async fn run_agent_register(
    state: &ServerState,
    params: AgentRegisterParams,
) -> Result<CallToolResult, McpError> {
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

pub(super) async fn run_agent_list(state: &ServerState, params: AgentListParams) -> Result<CallToolResult, McpError> {
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

pub(super) async fn run_thread_start(
    state: &ServerState,
    params: ThreadStartParams,
) -> Result<CallToolResult, McpError> {
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

pub(super) async fn run_thread_list(state: &ServerState, params: ThreadListParams) -> Result<CallToolResult, McpError> {
    let (remote, cwd) = scope_context_for(&state.root);
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

pub(super) async fn run_thread_join(state: &ServerState, params: ThreadJoinParams) -> Result<CallToolResult, McpError> {
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

pub(super) async fn run_thread_leave(
    state: &ServerState,
    params: ThreadLeaveParams,
) -> Result<CallToolResult, McpError> {
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

pub(super) async fn run_thread_members(
    state: &ServerState,
    params: ThreadMembersParams,
) -> Result<CallToolResult, McpError> {
    let label = params.thread.as_str().to_string();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    let members = client.thread_members(params.thread).await.map_err(comms_err)?;
    json_result(&ThreadMembersResponse {
        thread: label,
        members: members.iter().map(|m| m.as_str().to_string()).collect(),
    })
}

pub(super) async fn run_thread_add_member(
    state: &ServerState,
    params: ThreadMemberParams,
) -> Result<CallToolResult, McpError> {
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

pub(super) async fn run_thread_remove_member(
    state: &ServerState,
    params: ThreadMemberParams,
) -> Result<CallToolResult, McpError> {
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

pub(super) async fn run_thread_archive(
    state: &ServerState,
    params: ThreadArchiveParams,
) -> Result<CallToolResult, McpError> {
    let label = params.thread.as_str().to_string();
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    client.archive_thread(params.thread).await.map_err(comms_err)?;
    json_result(&ThreadArchiveResponse {
        thread: label,
        archived: true,
    })
}

pub(super) async fn run_thread_post(state: &ServerState, params: ThreadPostParams) -> Result<CallToolResult, McpError> {
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

pub(super) async fn run_thread_history(
    state: &ServerState,
    params: ThreadHistoryParams,
) -> Result<CallToolResult, McpError> {
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

pub(super) async fn run_message_get(state: &ServerState, params: MessageGetParams) -> Result<CallToolResult, McpError> {
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

pub(super) async fn run_inbox_read(state: &ServerState, params: InboxReadParams) -> Result<CallToolResult, McpError> {
    let limit = clamp_limit(params.limit);
    let cursor = params.cursor.map(crate::comms::cursor::Cursor);
    let since = since_cutoff(params.since_hours);
    let (remote, cwd) = scope_context_for(&state.root);
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

pub(super) async fn run_inbox_ack(state: &ServerState, params: InboxAckParams) -> Result<CallToolResult, McpError> {
    let has_bulk = params.thread.is_some() && params.to_seq.is_some();
    if params.message_ids.is_empty() && !has_bulk {
        return Err(comms_err("inbox_ack requires message_ids or a (thread, to_seq) pair"));
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
pub(super) async fn run_inbox_wait(state: &ServerState, params: InboxWaitParams) -> Result<CallToolResult, McpError> {
    let timeout_secs = params.timeout_secs.unwrap_or(DEFAULT_WAIT_SECS).clamp(1, MAX_WAIT_SECS);
    let cursor = params.cursor.map(crate::comms::cursor::Cursor);
    let since = since_cutoff(params.since_hours);
    let (remote, cwd) = scope_context_for(&state.root);

    let agent = match &params.as_agent {
        Some(raw) => AgentId::parse(raw.clone()).map_err(|e| comms_err(format!("invalid as_agent {raw:?}: {e}")))?,
        None => AgentId::parse(state.agent_id.clone())
            .map_err(|e| comms_err(format!("invalid agent id {:?}: {e}", state.agent_id)))?,
    };
    let mut client = CommsClient::ensure_and_connect(agent, remote.clone(), cwd.clone())
        .await
        .map_err(comms_err)?;

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
