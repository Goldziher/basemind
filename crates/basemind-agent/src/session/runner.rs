//! The session runner: the outer loop that ties [`run_turn`](super::run_turn) to the transport.
//!
//! It owns the session state (history, tools, provider pool, permission engine) and drives one turn
//! per `UserMessage` command, streaming events back over the [`EngineEndpoint`]. A future slice adds
//! a prompt queue and mid-turn cancellation; for now a message that arrives mid-turn is not queued.

use std::path::PathBuf;
use std::sync::Arc;

use basemind::mcp::BasemindServer;

use super::turn::{TurnContext, run_turn};
use crate::command::AgentCommand;
use crate::config::{AgentConfig, Role};
use crate::error::Result;
use crate::history::History;
use crate::permission::PermissionEngine;
use crate::provider::ProviderPool;
use crate::tools::ToolRegistry;
use crate::transport::EngineEndpoint;

/// One agent session: state plus the run-loop.
pub struct Session {
    history: History,
    tools: ToolRegistry,
    provider: ProviderPool,
    permission: PermissionEngine,
    root: PathBuf,
    server: Option<Arc<BasemindServer>>,
    role: Role,
    max_steps: u32,
    turn: u64,
}

impl Session {
    /// Build a session from config. The provider pool is resolved up front (so a bad model config
    /// fails fast); the permission engine starts from the deny-by-default base ruleset.
    pub fn new(
        config: &AgentConfig,
        root: PathBuf,
        server: Option<Arc<BasemindServer>>,
        tools: ToolRegistry,
        system_prompt: Option<String>,
    ) -> Result<Self> {
        Ok(Self {
            history: History::new(system_prompt),
            tools,
            provider: ProviderPool::from_config(config)?,
            permission: PermissionEngine::with_base(),
            root,
            server,
            role: Role::Default,
            max_steps: config.max_steps,
            turn: 0,
        })
    }

    /// Run until the command channel closes or a `Shutdown` arrives. Each `UserMessage` drives one
    /// turn; permission replies are consumed inside the turn.
    pub async fn run(mut self, endpoint: EngineEndpoint) {
        let EngineEndpoint { mut commands, events } = endpoint;
        while let Some(command) = commands.recv().await {
            match command {
                AgentCommand::UserMessage { text } => {
                    self.history.push_user(text);
                    self.turn += 1;
                    let resolved = self.provider.for_role(self.role).clone();
                    let mut cx = TurnContext {
                        history: &mut self.history,
                        tools: &self.tools,
                        role: &resolved,
                        permission: &self.permission,
                        root: self.root.clone(),
                        server: self.server.clone(),
                        max_steps: self.max_steps,
                    };
                    run_turn(self.turn, &mut cx, &events, &mut commands).await;
                }
                AgentCommand::Shutdown => break,
                // A permission reply or cancel with no turn in flight has nothing to answer.
                AgentCommand::PermissionDecision { .. } | AgentCommand::Cancel => {}
            }
        }
    }
}
