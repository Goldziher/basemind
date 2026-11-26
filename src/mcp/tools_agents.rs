//! The three agent tools of `BasemindServer` (ADR-0021): `agent_post`, `agent_inbox` and
//! `agent_threads`, thin shims over [`agent_tools`](super::agent_tools).
//!
//! Registered in a `comms` build only, and served by the daemon only: a daemon-hosted session
//! carries an [`AgentContext`](super::AgentContext) naming the store and who the session is, and a
//! server without one refuses with how to get one. Each store call runs on the blocking pool, since
//! the store is synchronous. A waiting read waits on the async side and holds no blocking thread:
//! the daemon's pool is bounded, and waiters parked there would starve every other request.
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
    AgentSession, AgentThreadsParams, finish_agent_inbox, inbox_refusal, prepare_agent_inbox, run_agent_post,
    run_agent_threads,
};
use crate::dispatch::json_result;
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
            self.on_agents(key, move |ops, session| run_agent_post(ops, session, params))
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
        self.answer(args, |params| async move {
            let context = self.agent_context()?;
            context.hub.opened().await;
            let notices = context.take_notices();
            let session = AgentSession {
                principal: &context.principal,
                repo: &context.repo,
                replay_key: key.as_ref(),
                notices: &notices,
            };
            let read = async {
                let (caller, call) = prepare_agent_inbox(&session, params)?;
                let reply = context
                    .hub
                    .read(caller, call.body, call.wait, &gone)
                    .await
                    .map_err(|error| inbox_refusal(&error))?;
                Ok::<_, McpError>(finish_agent_inbox(&session, reply))
            };
            match read.await {
                Ok(reply) => json_result(&reply),
                Err(error) => {
                    context.restore_notices(notices);
                    Err(error)
                }
            }
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
            self.on_agents(key, move |ops, session| run_agent_threads(ops, session, params))
        })
        .await
    }
}

impl BasemindServer {
    /// The session's agent context, or the refusal of a server the daemon does not host.
    fn agent_context(&self) -> Result<Arc<super::AgentContext>, McpError> {
        self.state.agents.clone().ok_or_else(|| {
            McpError::invalid_params(
                CommsError::Backend {
                    detail: "agent messaging is served by the basemind daemon, which hosts every session the \
                             `basemind serve` relay starts; this server runs without it (in-process or over HTTP). \
                             Reconnect the MCP server through `basemind serve`."
                        .to_string(),
                }
                .to_string(),
                None,
            )
        })
    }

    /// Run one agent-tool body against the daemon's store on the blocking pool, as this session.
    async fn on_agents<T, F>(&self, replay_key: Option<ReplayKey>, body: F) -> Result<CallToolResult, McpError>
    where
        T: serde::Serialize + Send + 'static,
        F: FnOnce(&dyn crate::comms::v2::api::AgentOps, &AgentSession<'_>) -> Result<T, McpError> + Send + 'static,
    {
        let context = self.agent_context()?;
        context.hub.opened().await;
        let reply = tokio::task::spawn_blocking(move || {
            let notices = context.take_notices();
            let session = AgentSession {
                principal: &context.principal,
                repo: &context.repo,
                replay_key: replay_key.as_ref(),
                notices: &notices,
            };
            let reply = body(&context.hub.ops(), &session);
            if reply.is_err() {
                context.restore_notices(notices);
            }
            reply
        })
        .await
        .map_err(|error| McpError::internal_error(format!("agent tool did not run: {error}"), None))??;
        json_result(&reply)
    }
}
