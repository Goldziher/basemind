//! Agent-comms tool shims for `BasemindServer`.
//!
//! Kept in a separate file so `tools.rs` stays under the 1000-line cap. Each shim is a thin
//! wrapper that delegates to a `helpers_comms::run_*` body and records telemetry. The whole router
//! is registered only under `#[cfg(feature = "comms")]`.

#![cfg(all(feature = "comms", any(unix, windows)))]

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::types_comms::{
    AgentListParams, AgentRegisterParams, InboxAckParams, InboxReadParams, MessageGetParams, ThreadArchiveParams,
    ThreadHistoryParams, ThreadJoinParams, ThreadLeaveParams, ThreadListParams, ThreadMemberParams,
    ThreadMembersParams, ThreadPostParams, ThreadStartParams,
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
        record_call(&self.state, "agent_register", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "List agents known to the comms broker, optionally restricted to the \
        members of one thread. Returns front-matter (id, card fields, first/last seen). \
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
        record_call(&self.state, "agent_list", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Start a conversation THREAD addressed by AT LEAST TWO of `subject` (topic \
        string), `path` (a path or globset glob like `src/**`), and `members` (explicit agent ids). \
        Fewer than two is rejected. You become the creator and a member. Discovery is scoped — a \
        thread is visible only to members, to agents whose cwd matches its path glob, or via a \
        subject filter; there is no global listing and no auto-join. Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn thread_start(
        &self,
        Parameters(p): Parameters<ThreadStartParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_thread_start(&self.state, p).await;
        record_call(&self.state, "thread_start", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "List threads DISCOVERABLE to this agent: those it is a member of, those \
        whose path glob matches this server's cwd, or (with `subject_contains`) those whose subject \
        contains the filter. NEVER all threads. Archived threads are excluded unless \
        `include_archived`. Returns front-matter (id, subject, path, members, creator, active, \
        last_activity, stale). Needs --features comms.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn thread_list(
        &self,
        Parameters(p): Parameters<ThreadListParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_thread_list(&self.state, p).await;
        record_call(&self.state, "thread_list", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Join a thread (durable membership; drives the inbox). Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn thread_join(
        &self,
        Parameters(p): Parameters<ThreadJoinParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_thread_join(&self.state, p).await;
        record_call(&self.state, "thread_join", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Leave a thread you are a member of. Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn thread_leave(
        &self,
        Parameters(p): Parameters<ThreadLeaveParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_thread_leave(&self.state, p).await;
        record_call(&self.state, "thread_leave", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "List the members of a thread. Needs --features comms.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn thread_members(
        &self,
        Parameters(p): Parameters<ThreadMembersParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_thread_members(&self.state, p).await;
        record_call(&self.state, "thread_members", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Add a member to a thread. Only the thread CREATOR may do this. \
        Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn thread_add_member(
        &self,
        Parameters(p): Parameters<ThreadMemberParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_thread_add_member(&self.state, p).await;
        record_call(&self.state, "thread_add_member", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Remove a member from a thread. Only the thread CREATOR may do this. \
        Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn thread_remove_member(
        &self,
        Parameters(p): Parameters<ThreadMemberParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_thread_remove_member(&self.state, p).await;
        record_call(
            &self.state,
            "thread_remove_member",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Archive a thread. Only the thread CREATOR (or a human via the CLI) may do \
        this; the system also auto-archives idle threads. Archived threads drop out of active \
        listings but their history stays readable. Idempotent. Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn thread_archive(
        &self,
        Parameters(p): Parameters<ThreadArchiveParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_thread_archive(&self.state, p).await;
        record_call(&self.state, "thread_archive", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Post a message (subject + optional markdown body + tags + reply_to) to a \
        thread. Returns the new message_id. The body is stored separately from front-matter. \
        Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn thread_post(
        &self,
        Parameters(p): Parameters<ThreadPostParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_thread_post(&self.state, p).await;
        record_call(&self.state, "thread_post", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Read a thread's history oldest-first, FRONT-MATTER ONLY (id, from, subject, \
        ts, age_secs, tags) — bodies are NOT included; fetch them with message_get. Defaults to the \
        last 24h of messages; pass `since_hours` for a different window or `since_hours=0` for ALL \
        history. Paginated: pass `cursor` from the previous response. Default 100 max 1000. \
        Needs --features comms.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn thread_history(
        &self,
        Parameters(p): Parameters<ThreadHistoryParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result = super::helpers_comms::run_thread_history(&self.state, p).await;
        record_call(&self.state, "thread_history", &__params_json, __started, &__result);
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
        record_call(&self.state, "message_get", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Read this agent's inbox: new FRONT-MATTER across all JOINED threads \
        (bodies NOT included — use message_get). Each row carries `age_secs`. Defaults to the last \
        24h; pass `since_hours` for a different window or `since_hours=0` for ALL unread. \
        `mark_read=true` advances read cursors. Returns the page plus remaining unread count. \
        Default 100 max 1000. Needs --features comms.",
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
        record_call(&self.state, "inbox_read", &__params_json, __started, &__result);
        __result
    }

    #[tool(
        description = "Acknowledge inbox messages by ADVANCING this agent's per-thread read \
        cursors — it does NOT delete anything and does NOT affect the shared log or any other \
        agent's inbox. Two modes, combinable: pass `message_ids` to ack specific messages, and/or \
        `thread` + `to_seq` to bulk-ack everything up to `to_seq` in one thread. At least one mode \
        is required. Needs --features comms.",
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
        record_call(&self.state, "inbox_ack", &__params_json, __started, &__result);
        __result
    }
}
