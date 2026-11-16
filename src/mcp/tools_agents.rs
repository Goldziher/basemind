//! The three agent tools of `BasemindServer` (ADR-0021): `agent_post`, `agent_inbox` and
//! `agent_threads`, thin shims over [`agent_tools`](super::agent_tools).
//!
//! Registered in a `comms` build only, and served by the daemon only: a daemon-hosted session
//! carries an [`AgentContext`](super::AgentContext) naming the store and who the session is, and a
//! server without one refuses with how to get one. The bodies run on the blocking pool, since the
//! store is synchronous; a waiting read blocks there on the daemon's wait.
//!
//! The replay key comes from the relay, in the request's `_meta` under [`REPLAY_KEY_META`], never
//! from an argument.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::RoleServer;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::tool;

use super::BasemindServer;
use super::agent_tools::{
    AGENT_INBOX_DESCRIPTION, AGENT_POST_DESCRIPTION, AGENT_THREADS_DESCRIPTION, AgentInboxParams, AgentPostParams,
    AgentSession, AgentThreadsParams, run_agent_inbox, run_agent_post, run_agent_threads,
};
use super::helpers::json_result;
use super::replay::REPLAY_KEY_META;
use super::strict::Strict;
use crate::comms::v2::api::{CommsError, ReplayKey};

/// The replay key the relay stamped on this request, when it is one.
fn replay_key(ctx: &RequestContext<RoleServer>) -> Option<ReplayKey> {
    ctx.meta
        .get(REPLAY_KEY_META)
        .and_then(serde_json::Value::as_str)
        .and_then(|key| ReplayKey::parse(key).ok())
}

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_agents")]
impl BasemindServer {
    #[tool(
        title = "Message agents",
        description = AGENT_POST_DESCRIPTION,
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn agent_post(
        &self,
        Parameters(args): Parameters<Strict<AgentPostParams>>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let key = replay_key(&ctx);
        self.answer(args, |params| {
            self.on_agents(key, move |ops, session, _| run_agent_post(ops, session, params))
        })
        .await
    }

    #[tool(
        title = "Read agent messages",
        description = AGENT_INBOX_DESCRIPTION,
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn agent_inbox(
        &self,
        Parameters(args): Parameters<Strict<AgentInboxParams>>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let key = replay_key(&ctx);
        let gone = ctx.ct.clone();
        self.answer(args, |params| {
            self.on_agents(key, move |ops, session, context| {
                let runtime = tokio::runtime::Handle::current();
                let hub = Arc::clone(&context.hub);
                let wait = move |caller: &crate::comms::v2::api::Caller, body, wait| {
                    runtime.block_on(hub.read(caller.clone(), body, wait, &gone))
                };
                run_agent_inbox(ops, session, params, &wait)
            })
        })
        .await
    }

    #[tool(
        title = "Agent threads",
        description = AGENT_THREADS_DESCRIPTION,
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn agent_threads(
        &self,
        Parameters(args): Parameters<Strict<AgentThreadsParams>>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let key = replay_key(&ctx);
        self.answer(args, |params| {
            self.on_agents(key, move |ops, session, _| run_agent_threads(ops, session, params))
        })
        .await
    }
}

impl BasemindServer {
    /// Run one agent-tool body against the daemon's store on the blocking pool, as this session.
    async fn on_agents<T, F>(&self, replay_key: Option<ReplayKey>, body: F) -> Result<CallToolResult, McpError>
    where
        T: serde::Serialize + Send + 'static,
        F: FnOnce(&dyn crate::comms::v2::api::AgentOps, &AgentSession<'_>, &super::AgentContext) -> Result<T, McpError>
            + Send
            + 'static,
    {
        let Some(context) = self.state.agents.clone() else {
            return Err(McpError::invalid_params(
                CommsError::Backend {
                    detail: "agent messaging is served by the basemind daemon: start this server with `basemind serve`"
                        .to_string(),
                }
                .to_string(),
                None,
            ));
        };
        let reply = tokio::task::spawn_blocking(move || {
            let notices = context.take_notices();
            let session = AgentSession {
                principal: &context.principal,
                repo: &context.repo,
                replay_key: replay_key.as_ref(),
                notices: &notices,
            };
            body(&context.hub.ops(), &session, &context)
        })
        .await
        .map_err(|error| McpError::internal_error(format!("agent tool did not run: {error}"), None))??;
        json_result(&reply)
    }
}
