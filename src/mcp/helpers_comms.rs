//! Helper bodies for the agent-comms MCP tools.
//!
//! Each `run_<tool>` is a thin proxy: acquire the lazily-connected
//! [`CommsClient`](crate::comms::client::CommsClient) from [`ServerState`], inject the server's
//! resolved scope context (and identity, already baked into the connected client), call the
//! matching client method, and `json_result` the front-matter response. History and inbox
//! tools surface front-matter ONLY — bodies are fetched exclusively through `message_get`.

#![cfg(all(feature = "comms", unix))]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use tokio::sync::MutexGuard;

use super::ServerState;
use super::helpers::json_result;
use super::types_comms::{
    AgentListParams, AgentListResponse, AgentRegisterParams, AgentRegisterResponse, AgentSummary,
    CursorAdvance, InboxAckParams, InboxAckResponse, InboxReadParams, InboxReadResponse,
    MessageFrontMatter, MessageGetParams, MessageGetResponse, RoomCreateParams, RoomCreateResponse,
    RoomHistoryParams, RoomHistoryResponse, RoomJoinParams, RoomLeaveParams, RoomListParams,
    RoomListResponse, RoomMembershipResponse, RoomPostParams, RoomPostResponse, RoomSummary,
};
use crate::comms::client::{CommsClient, scope_context_for};
use crate::comms::cursor::Cursor;

/// Default page size when a comms tool omits `limit`. Mirrors the broker's `DEFAULT_LIMIT`.
const DEFAULT_LIMIT: u32 = 100;

/// Map a [`CommsClientError`](crate::comms::client::CommsClientError) into an MCP error with a
/// stable `comms:` prefix so agents can route on it.
fn comms_err(error: impl std::fmt::Display) -> McpError {
    McpError::internal_error(format!("comms: {error}"), None)
}

/// Lazily connect (spawning the daemon on first use) and return a guard over the cached client.
///
/// The connection is keyed to the server's resolved `agent_id` (already validated through
/// `AgentId` at boot — "anon" is valid) and the server root's scope context. Connecting is
/// best-effort: a failure surfaces here as an MCP error on the triggering call, never at boot.
async fn comms_client(
    state: &ServerState,
) -> Result<MutexGuard<'_, Option<CommsClient>>, McpError> {
    let mut guard = state.comms_client.lock().await;
    if guard.is_none() {
        let agent = crate::comms::ids::AgentId::parse(state.agent_id.clone())
            .map_err(|e| comms_err(format!("invalid agent id {:?}: {e}", state.agent_id)))?;
        let (remote, cwd) = scope_context_for(&state.root);
        let client = CommsClient::ensure_and_connect(agent, remote, cwd)
            .await
            .map_err(comms_err)?;
        *guard = Some(client);
    }
    Ok(guard)
}

/// Borrow the connected client out of the guard. Infallible after [`comms_client`] returned Ok.
fn client_mut(guard: &mut Option<CommsClient>) -> Result<&mut CommsClient, McpError> {
    guard
        .as_mut()
        .ok_or_else(|| comms_err("client unexpectedly disconnected"))
}

/// Clamp a caller-supplied limit to `[1, MAX_LIMIT]`, defaulting when absent.
fn clamp_limit(limit: Option<u32>) -> u32 {
    limit
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, crate::comms::daemon::MAX_LIMIT)
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
    let mut guard = comms_client(state).await?;
    let client = client_mut(&mut guard)?;
    let agent_id = client.agent().as_str().to_string();
    client.register_agent(card).await.map_err(comms_err)?;
    json_result(&AgentRegisterResponse {
        agent_id,
        registered: true,
    })
}

pub(super) async fn run_agent_list(
    state: &ServerState,
    params: AgentListParams,
) -> Result<CallToolResult, McpError> {
    let mut guard = comms_client(state).await?;
    let client = client_mut(&mut guard)?;
    let records = client.list_agents(params.room).await.map_err(comms_err)?;
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

pub(super) async fn run_room_create(
    state: &ServerState,
    params: RoomCreateParams,
) -> Result<CallToolResult, McpError> {
    let scope = params.scope.into();
    let mut guard = comms_client(state).await?;
    let client = client_mut(&mut guard)?;
    let room = client
        .create_room(params.room, scope, params.title)
        .await
        .map_err(comms_err)?;
    json_result(&RoomCreateResponse {
        room: RoomSummary::from(&room),
    })
}

pub(super) async fn run_room_list(
    state: &ServerState,
    _params: RoomListParams,
) -> Result<CallToolResult, McpError> {
    let (remote, cwd) = scope_context_for(&state.root);
    let mut guard = comms_client(state).await?;
    let client = client_mut(&mut guard)?;
    let rooms = client.list_rooms(remote, cwd).await.map_err(comms_err)?;
    let summaries: Vec<RoomSummary> = rooms.iter().map(RoomSummary::from).collect();
    json_result(&RoomListResponse {
        total: summaries.len(),
        rooms: summaries,
    })
}

pub(super) async fn run_room_join(
    state: &ServerState,
    params: RoomJoinParams,
) -> Result<CallToolResult, McpError> {
    let room_label = params.room.as_str().to_string();
    let mut guard = comms_client(state).await?;
    let client = client_mut(&mut guard)?;
    client.join_room(params.room).await.map_err(comms_err)?;
    json_result(&RoomMembershipResponse {
        room: room_label,
        joined: true,
        left: false,
    })
}

pub(super) async fn run_room_leave(
    state: &ServerState,
    params: RoomLeaveParams,
) -> Result<CallToolResult, McpError> {
    let room_label = params.room.as_str().to_string();
    let mut guard = comms_client(state).await?;
    let client = client_mut(&mut guard)?;
    client.leave_room(params.room).await.map_err(comms_err)?;
    json_result(&RoomMembershipResponse {
        room: room_label,
        joined: false,
        left: true,
    })
}

pub(super) async fn run_room_post(
    state: &ServerState,
    params: RoomPostParams,
) -> Result<CallToolResult, McpError> {
    let body = params.body.unwrap_or_default().into_bytes();
    let tags = params.tags.unwrap_or_default();
    let scope = params.scope.unwrap_or_default();
    let mut guard = comms_client(state).await?;
    let client = client_mut(&mut guard)?;
    let message_id = client
        .post_message(
            params.room,
            params.subject,
            body,
            tags,
            params.reply_to,
            scope,
        )
        .await
        .map_err(comms_err)?;
    json_result(&RoomPostResponse { message_id })
}

pub(super) async fn run_room_history(
    state: &ServerState,
    params: RoomHistoryParams,
) -> Result<CallToolResult, McpError> {
    let limit = clamp_limit(params.limit);
    let cursor = params.cursor.map(Cursor);
    let mut guard = comms_client(state).await?;
    let client = client_mut(&mut guard)?;
    let (metas, next_cursor) = client
        .read_history(params.room, cursor, limit)
        .await
        .map_err(comms_err)?;
    let messages: Vec<MessageFrontMatter> = metas.iter().map(MessageFrontMatter::from).collect();
    json_result(&RoomHistoryResponse {
        total: messages.len(),
        messages,
        next_cursor,
    })
}

pub(super) async fn run_message_get(
    state: &ServerState,
    params: MessageGetParams,
) -> Result<CallToolResult, McpError> {
    let message_id = params.message_id.clone();
    let mut guard = comms_client(state).await?;
    let client = client_mut(&mut guard)?;
    let body = client
        .get_body(params.message_id)
        .await
        .map_err(comms_err)?;
    let found = body.is_some();
    let body = body.map(|b| String::from_utf8_lossy(&b).into_owned());
    json_result(&MessageGetResponse {
        message_id,
        found,
        body,
    })
}

pub(super) async fn run_inbox_read(
    state: &ServerState,
    params: InboxReadParams,
) -> Result<CallToolResult, McpError> {
    let limit = clamp_limit(params.limit);
    let cursor = params.cursor.map(Cursor);
    let (remote, cwd) = scope_context_for(&state.root);
    let mut guard = comms_client(state).await?;
    let client = client_mut(&mut guard)?;
    let (metas, unread, next_cursor) = client
        .read_inbox(remote, cwd, cursor, limit, params.mark_read)
        .await
        .map_err(comms_err)?;
    let messages: Vec<MessageFrontMatter> = metas.iter().map(MessageFrontMatter::from).collect();
    json_result(&InboxReadResponse {
        total: messages.len(),
        unread,
        messages,
        next_cursor,
    })
}

pub(super) async fn run_inbox_ack(
    state: &ServerState,
    params: InboxAckParams,
) -> Result<CallToolResult, McpError> {
    // Validate up front: at least one mode must be supplied (the broker also guards this, but
    // failing fast here gives a clearer MCP error than a round-trip).
    let has_bulk = params.room.is_some() && params.to_seq.is_some();
    if params.message_ids.is_empty() && !has_bulk {
        return Err(comms_err(
            "inbox_ack requires message_ids or a (room, to_seq) pair",
        ));
    }
    let mut guard = comms_client(state).await?;
    let client = client_mut(&mut guard)?;
    let (acked, cursors) = client
        .ack_inbox(params.message_ids, params.room, params.to_seq)
        .await
        .map_err(comms_err)?;
    let cursors_advanced: Vec<CursorAdvance> = cursors
        .into_iter()
        .map(|(room, seq)| CursorAdvance { room, seq })
        .collect();
    json_result(&InboxAckResponse {
        acked: acked as usize,
        cursors_advanced,
    })
}
