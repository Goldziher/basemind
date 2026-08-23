//! The `agents` domain tool shim for `BasemindServer`.
//!
//! One tool, one required `mode` — `register` / `list` / `thread_start` / `thread_list` / `join` /
//! `leave` / `members` / `add_member` / `remove_member` / `archive` / `post` / `history` /
//! `message` / `inbox` / `ack` / `wait` — dispatched to `helpers_comms::run_agents`. Thin wrapper:
//! the bodies live in `helpers_comms.rs`. The whole module is gated on `feature = "comms"`, because
//! every mode talks to the user-global broker daemon: with the feature off there is no broker and
//! the tool is never registered rather than answering with a `not_enabled` stub.

#![cfg(all(feature = "comms", any(unix, windows)))]

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::lenient::Lenient;
use super::types_comms::AgentsParams;

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_comms")]
impl BasemindServer {
    // No `output_schema`: the sixteen modes return a dozen different response shapes, and SEP-2106
    // allows exactly one per tool. Declaring a union would mean nested structs, which schemars emits
    // as `$ref` into `$defs` — the construct that silently dropped the whole registry in GH #50. The
    // per-mode shapes are documented in the description instead. ~keep
    //
    // The annotations are the union of the sixteen modes': `post` / `join` / `register` write, and a
    // post is not idempotent, so the tool can claim neither `read_only_hint` nor `idempotent_hint`.
    // Nothing here destroys data — `ack` advances a private read cursor and `archive` is
    // reversible-by-reading. ~keep
    #[tool(
        description = "Talk to the other agents working this repository: message another agent, \
        find out who else is here, ask a question and get a reply, and check your inbox. Use it \
        when you start, finish, hit a decision, or are about to touch code someone else may already \
        be changing. `mode` is required. Coordination runs over THREADS — scoped conversations, \
        never a global chat room. `thread_start` opens one, addressed by AT LEAST TWO of `subject` \
        (topic), `path` (a path or globset glob like `src/**`), and `members` (explicit agent ids); \
        fewer than two is rejected, and you become the creator and a member. `thread_list` shows \
        only the threads DISCOVERABLE to you — ones you are a member of, ones whose path glob \
        matches this server's cwd, or (with `subject_contains`) a subject filter; there is no \
        global listing, and archived threads are excluded unless `include_archived`. `join` a \
        thread to route its messages into your inbox and `leave` to stop: membership is always \
        explicit, there is NO auto-join. `post` sends a message (`thread` + `subject`, plus \
        optional markdown `body`, `tags`, and `reply_to` to reply to a specific message) and \
        returns its `message_id`. `history` reads one thread oldest-first and `inbox` reads what is \
        new across every thread you joined; BOTH return FRONT-MATTER ONLY (message_ref, legacy id, from, subject, \
        ts, age_secs, tags, seq, body_len) — the body is never included, and mode `message` fetching \
        one compact `message_ref` (preferred) or legacy `message_id` is the only path to a body, \
        so scan the front-matter and fetch only the \
        bodies you need. Both default to the last 24h (`since_hours`, `0` for all history) and \
        paginate (`limit` default 100 max 1000, `cursor` from the previous `next_cursor`). `wait` \
        long-polls up to `timeout_secs` (default 30, max 300) and returns the moment a peer posts, \
        instead of you looping `inbox`; it never marks anything read. `ack` clears messages you \
        have handled by ADVANCING your own per-thread read cursors (`message_ids`, and/or `thread` \
        + `to_seq`) — it deletes nothing and touches no other agent's inbox. `register` publishes \
        this agent's card (name / description / version / skills) so peers can see who you are, and \
        `list` shows the agents the broker knows, optionally just one thread's members. `members` \
        lists a thread's membership; `add_member` / `remove_member` / `archive` are creator-only \
        (a human admin can archive too, and idle threads auto-archive). Every mode takes \
        `as_agent` to act as a named sub-identity, so one orchestrator can drive several agents \
        with separate inboxes. Parameters that belong to another mode are rejected, not ignored. \
        Needs --features comms.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn agents(
        &self,
        Parameters(Lenient(p)): Parameters<Lenient<AgentsParams>>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __key = p.mode.telemetry_key();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> = super::helpers_comms::run_agents(&self.state, p).await;
        record_call(&self.state, __key, &__params_json, __started, &__result);
        __result
    }
}
