//! Agent-comms tool shims for `BasemindServer`.
//!
//! Kept in a separate file so `tools.rs` stays under the 1000-line cap. Each shim is a thin
//! wrapper that delegates to a `helpers_comms::run_*` body and records telemetry; when the
//! `comms` feature is off it returns a graceful "not enabled" MCP error (mirroring the memory
//! tier). The whole router is registered only under `#[cfg(feature = "comms")]`.

#![cfg(all(feature = "comms", unix))]

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::types_comms::{
    AgentListParams, AgentRegisterParams, DmSendParams, GetOrCreateRoomForPathParams,
    InboxAckParams, InboxReadParams, MessageGetParams, RoomCreateParams, RoomHistoryParams,
    RoomJoinParams, RoomLeaveParams, RoomListParams, RoomPostParams,
};

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_comms")]
impl BasemindServer {
    #[tool(
        description = "Register or update this agent's A2A card (name/description/version/skills) \
        with the user-global comms broker. Spawns the broker daemon on first use. \
        Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn agent_register(
        &self,
        Parameters(p): Parameters<AgentRegisterParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_agent_register(&self.state, p).await;
        record_call(
            &self.state,
            "agent_register",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "List agents known to the comms broker, optionally restricted to the \
        subscribers of one room. Returns front-matter (id, card fields, first/last seen). \
        Needs --features comms.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn agent_list(
        &self,
        Parameters(p): Parameters<AgentListParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_agent_list(&self.state, p).await;
        record_call(
            &self.state,
            "agent_list",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Create (and register) a comms room with an explicit scope: `remote` (a git \
        remote — every clone auto-joins), `path_prefix` (agents at/below a path auto-join), or \
        `global` (every agent on the machine — reserve it for MACHINE-WIDE ops coordination like \
        CPU / resource contention, NOT per-repo chat). For work in a repo prefer \
        get_or_create_chat_room_for_path / a repo room. Idempotent. Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn room_create(
        &self,
        Parameters(p): Parameters<RoomCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_room_create(&self.state, p).await;
        record_call(
            &self.state,
            "room_create",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "List rooms whose scope matches this server's repo (git remote + cwd). \
        Returns room front-matter (id, title, created_at, last_activity_micros, stale). A room is \
        `stale` when it has never had a post or its last post is older than 7 days — skip those. \
        Needs --features comms.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn room_list(
        &self,
        Parameters(p): Parameters<RoomListParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_room_list(&self.state, p).await;
        record_call(
            &self.state,
            "room_list",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Subscribe this agent to a room (durable membership; drives the inbox). \
        Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn room_join(
        &self,
        Parameters(p): Parameters<RoomJoinParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_room_join(&self.state, p).await;
        record_call(
            &self.state,
            "room_join",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Unsubscribe this agent from a room. Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn room_leave(
        &self,
        Parameters(p): Parameters<RoomLeaveParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_room_leave(&self.state, p).await;
        record_call(
            &self.state,
            "room_leave",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Post a message (subject + optional markdown body + tags + reply_to) to a \
        room. Returns the new message_id. The body is stored separately from front-matter. \
        Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn room_post(
        &self,
        Parameters(p): Parameters<RoomPostParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_room_post(&self.state, p).await;
        record_call(
            &self.state,
            "room_post",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Send a DIRECT message to one agent's inbox via a private pairwise room \
        (`dm:<lo>:<hi>`) that both ends auto-join. Optional `as_agent` sends on behalf of a \
        subagent. The recipient sees it in inbox_read(as_agent=<recipient>). Returns the \
        message_id and the room. Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn dm_send(
        &self,
        Parameters(p): Parameters<DmSendParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_dm_send(&self.state, p).await;
        record_call(&self.state, "dm_send", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Resolve the repo at `path` to its CANONICAL room (keyed by git remote, else \
        the repo root path) and join it — get-or-create. Use this to coordinate in ANOTHER repo's \
        room: it returns the same room agents working in that repo auto-join. Any subdirectory of a \
        repo maps to one room. Optional `as_agent` joins on behalf of a subagent. Returns the room \
        id, scope label, title, and whether it was created. Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn get_or_create_chat_room_for_path(
        &self,
        Parameters(p): Parameters<GetOrCreateRoomForPathParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result =
            super::helpers_comms::run_get_or_create_chat_room_for_path(&self.state, p).await;
        record_call(
            &self.state,
            "get_or_create_chat_room_for_path",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Read a room's history oldest-first, FRONT-MATTER ONLY (id, from, subject, \
        ts, age_secs, tags) — bodies are NOT included; fetch them with message_get. Defaults to the \
        last 24h of messages so stale chatter does not drown out current work; pass `since_hours` \
        for a different window or `since_hours=0` for ALL history. Paginated: pass `cursor` from the \
        previous response. Default 100 max 1000. Needs --features comms.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn room_history(
        &self,
        Parameters(p): Parameters<RoomHistoryParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_room_history(&self.state, p).await;
        record_call(
            &self.state,
            "room_history",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Fetch a single message BODY by id (the only body path; history/inbox \
        return front-matter only). Body is returned as a UTF-8 (lossy) markdown string. \
        Needs --features comms.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn message_get(
        &self,
        Parameters(p): Parameters<MessageGetParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_message_get(&self.state, p).await;
        record_call(
            &self.state,
            "message_get",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Read this agent's inbox: new FRONT-MATTER across all subscribed rooms \
        (bodies NOT included — use message_get). Each row carries `age_secs` so you can gauge \
        staleness. Defaults to the last 24h; pass `since_hours` for a different window or \
        `since_hours=0` for ALL unread. `mark_read=true` advances read cursors. Returns the page \
        plus remaining unread count. Default 100 max 1000. Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn inbox_read(
        &self,
        Parameters(p): Parameters<InboxReadParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_inbox_read(&self.state, p).await;
        record_call(
            &self.state,
            "inbox_read",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Acknowledge inbox messages by ADVANCING this agent's per-room read cursors \
        — it does NOT delete anything and does NOT affect the shared append-only log or any other \
        agent's inbox (message_get and room_history still return acked messages). Two modes, \
        combinable: pass `message_ids` to ack specific messages (each resolved to its room+seq), \
        and/or `room` + `to_seq` to bulk-ack everything up to `to_seq` in one room (stale-room \
        cleanup). At least one mode is required. Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn inbox_ack(
        &self,
        Parameters(p): Parameters<InboxAckParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_inbox_ack(&self.state, p).await;
        record_call(
            &self.state,
            "inbox_ack",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }
}
