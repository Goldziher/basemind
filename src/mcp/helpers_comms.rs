//! Helper bodies for the agent-comms MCP tools.
//!
//! Each `run_<tool>` is a thin proxy: acquire the lazily-connected
//! [`CommsClient`](crate::comms::client::CommsClient) from [`ServerState`], inject the server's
//! resolved scope context (and identity, already baked into the connected client), call the
//! matching client method, and `json_result` the front-matter response. History and inbox
//! tools surface front-matter ONLY — bodies are fetched exclusively through `message_get`.

#![cfg(all(feature = "comms", unix))]

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use tokio::sync::Mutex;

use super::ServerState;
use super::helpers::json_result;
use super::types_comms::{
    AgentListParams, AgentListResponse, AgentRegisterParams, AgentRegisterResponse, AgentSummary,
    CursorAdvance, DmSendParams, DmSendResponse, GetOrCreateRoomForPathParams,
    GetOrCreateRoomForPathResponse, InboxAckParams, InboxAckResponse, InboxReadParams,
    InboxReadResponse, MessageFrontMatter, MessageGetParams, MessageGetResponse, RoomCreateParams,
    RoomCreateResponse, RoomHistoryParams, RoomHistoryResponse, RoomJoinParams, RoomLeaveParams,
    RoomListParams, RoomListResponse, RoomMembershipResponse, RoomPostParams, RoomPostResponse,
    RoomSummary,
};
use crate::comms::client::{CommsClient, SessionContext, scope_context_for};
use crate::comms::cursor::Cursor;
use crate::comms::ids::{AgentId, RoomId};
use crate::comms::model::RoomScope;

/// Default page size when a comms tool omits `limit`. Mirrors the broker's `DEFAULT_LIMIT`.
const DEFAULT_LIMIT: u32 = 100;

/// Map a [`CommsClientError`](crate::comms::client::CommsClientError) into an MCP error with a
/// stable `comms:` prefix so agents can route on it.
pub(super) fn comms_err(error: impl std::fmt::Display) -> McpError {
    McpError::internal_error(format!("comms: {error}"), None)
}

/// Resolve (lazily connecting + caching) the comms-broker client for the requested identity.
///
/// `as_agent` selects a sub-identity to act as; `None` resolves the server's own `agent_id`
/// (the pre-registry behavior). The server's OWN identity connects with its env-derived session
/// (today's behavior). A SUB-identity is parented to the server and shares the orchestration
/// session id so the broker records lineage and (optionally) auto-joins a session-scoped room.
///
/// The first connect for a given identity is serialized under the registry map lock — acceptable,
/// since it only blocks concurrent FIRST-connects of the SAME process, not steady-state traffic.
/// Each returned handle is an `Arc<Mutex<CommsClient>>`; callers `lock().await` it per call.
pub(super) async fn resolve_comms_client(
    state: &ServerState,
    as_agent: Option<String>,
) -> Result<Arc<Mutex<CommsClient>>, McpError> {
    // Target identity: as_agent (validated) or the server's own agent_id.
    let target = match as_agent {
        Some(raw) => AgentId::parse(raw.clone())
            .map_err(|e| comms_err(format!("invalid as_agent {raw:?}: {e}")))?,
        None => AgentId::parse(state.agent_id.clone())
            .map_err(|e| comms_err(format!("invalid agent id {:?}: {e}", state.agent_id)))?,
    };
    let mut map = state.comms_clients.lock().await;
    if let Some(handle) = map.get(&target) {
        return Ok(handle.clone());
    }
    let (remote, cwd) = scope_context_for(&state.root);
    let is_self = target.as_str() == state.agent_id;
    let client = if is_self {
        CommsClient::ensure_and_connect(target.clone(), remote, cwd)
            .await
            .map_err(comms_err)?
    } else {
        let session = SessionContext {
            session_id: Some(state.orchestration_session.clone()),
            parent_agent: Some(state.agent_id.clone()),
        };
        CommsClient::ensure_and_connect_with_session(target.clone(), remote, cwd, session)
            .await
            .map_err(comms_err)?
    };
    let handle = Arc::new(Mutex::new(client));
    map.insert(target, handle.clone());
    Ok(handle)
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
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
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
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
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
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
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
    let handle = resolve_comms_client(state, None).await?;
    let mut client = handle.lock().await;
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
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
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
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
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
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
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
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
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
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
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
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
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
    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
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

/// `get_or_create_chat_room_for_path`: resolve the repo at `params.path` to its canonical room and
/// join it, so an agent working in one repo can coordinate in ANOTHER repo's room.
///
/// The room id / scope / title come from [`repo_room_for`](crate::comms::daemon::repo_room_for) —
/// the SAME derivation the broker's auto-join uses — so this returns exactly the room agents in the
/// target repo auto-join (keyed by git remote when present, else the repo root path). `path` is
/// first resolved to the repo ROOT so any subdirectory maps to one room. `created` is computed by
/// listing the rooms matching that scope BEFORE the idempotent `create_room` upsert.
pub(super) async fn run_get_or_create_chat_room_for_path(
    state: &ServerState,
    params: GetOrCreateRoomForPathParams,
) -> Result<CallToolResult, McpError> {
    // Resolve the repo root so subdirs of one repo map to a single room; fall back to the raw path
    // when it is not inside a git repo (a path-scoped room keyed by the path itself).
    let base = crate::git::Repo::discover(std::path::Path::new(&params.path))
        .ok()
        .map(|r| r.workdir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from(&params.path));
    let (remote, cwd) = scope_context_for(&base);
    let room = crate::comms::daemon::repo_room_for(remote.clone(), cwd.clone());
    let scope_label = match &room.scope {
        RoomScope::Remote(_) => "remote",
        RoomScope::PathPrefix(_) => "path",
        RoomScope::Session(_) => "session",
        RoomScope::Global => "global",
    };

    let handle = resolve_comms_client(state, params.as_agent).await?;
    let mut client = handle.lock().await;
    // Existence check BEFORE the idempotent upsert: list the rooms matching the target scope and
    // see whether the derived id is already present.
    let existed = client
        .list_rooms(remote, cwd)
        .await
        .map_err(comms_err)?
        .iter()
        .any(|r| r.room_id == room.room_id);
    client
        .create_room(
            room.room_id.clone(),
            room.scope.clone(),
            Some(room.title.clone()),
        )
        .await
        .map_err(comms_err)?;
    client
        .join_room(room.room_id.clone())
        .await
        .map_err(comms_err)?;

    json_result(&GetOrCreateRoomForPathResponse {
        room: room.room_id.as_str().to_string(),
        scope: scope_label.to_string(),
        title: room.title,
        created: !existed,
    })
}

/// `dm_send`: deliver a direct message to one agent's inbox via a private pairwise room.
///
/// There is no broker-level DM primitive; instead the orchestrator (which hosts BOTH the sender's
/// and the recipient's broker connections in its [`resolve_comms_client`] registry) creates a
/// canonical pairwise room `dm:<lo>:<hi>` (the two ids sorted so both directions map to one room),
/// joins it on BOTH ends, and posts via the sender. The message then surfaces in the recipient's
/// `inbox_read(as_agent = to_agent)` like any other subscribed-room message. The recipient's
/// connection is created lazily if it does not exist yet.
pub(super) async fn run_dm_send(
    state: &ServerState,
    params: DmSendParams,
) -> Result<CallToolResult, McpError> {
    // Resolve both identities up front (the sender may be a sub-identity via `as_agent`).
    let from_agent = match &params.as_agent {
        Some(raw) => AgentId::parse(raw.clone())
            .map_err(|e| comms_err(format!("invalid as_agent {raw:?}: {e}")))?,
        None => AgentId::parse(state.agent_id.clone())
            .map_err(|e| comms_err(format!("invalid agent id {:?}: {e}", state.agent_id)))?,
    };
    let to_agent = AgentId::parse(params.to_agent.clone())
        .map_err(|e| comms_err(format!("invalid to_agent {:?}: {e}", params.to_agent)))?;
    if from_agent == to_agent {
        return Err(comms_err("cannot dm yourself"));
    }

    // Canonical pairwise room id: sort the two ids so a<->b and b<->a map to the same room.
    let (lo, hi) = if from_agent.as_str() <= to_agent.as_str() {
        (from_agent.as_str(), to_agent.as_str())
    } else {
        (to_agent.as_str(), from_agent.as_str())
    };
    let room = RoomId::parse(format!("dm:{lo}:{hi}"))
        .map_err(|e| comms_err(format!("derive dm room id: {e}")))?;

    // Ensure the room exists and the SENDER is subscribed (create_room is an idempotent upsert).
    // The room is scoped to a UNIQUE session token that matches no agent's real session id, so the
    // broker never auto-joins anyone — membership is explicit (only the two ends that `join_room`).
    // (A `Global` scope would broadcast the DM to every agent on the machine.)
    let dm_scope = RoomScope::Session(format!("dm:{lo}:{hi}"));
    let sender = resolve_comms_client(state, params.as_agent.clone()).await?;
    {
        let mut client = sender.lock().await;
        client
            .create_room(room.clone(), dm_scope, Some(format!("dm {lo} <-> {hi}")))
            .await
            .map_err(comms_err)?;
        client.join_room(room.clone()).await.map_err(comms_err)?;
    }

    // Subscribe the RECIPIENT via its own connection (lazily created) so the DM lands in its inbox.
    {
        let recipient = resolve_comms_client(state, Some(to_agent.as_str().to_string())).await?;
        let mut client = recipient.lock().await;
        client.join_room(room.clone()).await.map_err(comms_err)?;
    }

    // Post via the sender.
    let body = params.body.unwrap_or_default().into_bytes();
    let message_id = {
        let mut client = sender.lock().await;
        client
            .post_message(
                room.clone(),
                params.subject,
                body,
                Vec::new(),
                params.reply_to,
                Vec::new(),
            )
            .await
            .map_err(comms_err)?
    };

    json_result(&DmSendResponse {
        message_id,
        room: room.into_string(),
    })
}
