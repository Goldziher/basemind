//! The session runner: the outer loop that ties [`run_turn`](super::run_turn) to the transport.
//!
//! It owns the session state (history, tools, provider pool, permission engine) and drives one turn
//! per `UserMessage` command, streaming events back over the [`EngineEndpoint`]. A running turn is
//! cancellable mid-stream (the turn-loop polls the command channel while streaming and executing
//! tools); a `UserMessage` that arrives mid-turn is not yet queued.

use std::path::PathBuf;
use std::sync::Arc;

use basemind::mcp::BasemindServer;
use liter_llm::Message;

use super::turn::{TurnContext, run_turn};
use crate::command::AgentCommand;
use crate::config::{AgentConfig, Role};
use crate::error::Result;
use crate::event::AgentEvent;
use crate::history::{History, SessionMeta, SessionStore};
use crate::permission::PermissionEngine;
use crate::provider::ProviderPool;
use crate::tools::ToolRegistry;
use crate::transport::EngineEndpoint;
use tokio::sync::broadcast;

/// Maximum length, in characters, of a session title derived from the first user message.
const TITLE_MAX_CHARS: usize = 60;

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
    store: Option<SessionStore>,
    persisted: usize,
    title: Option<String>,
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
            store: None,
            persisted: 0,
            title: None,
        })
    }

    /// Persist this session's turns to `store`, appending new messages between turns.
    pub fn persist_to(mut self, store: SessionStore) -> Self {
        self.store = Some(store);
        self
    }

    /// Seed the history from a resumed session: append `messages`, set the token totals, and mark
    /// them already-persisted so they are not re-appended on the next turn.
    pub fn seed(mut self, messages: Vec<Message>, input_tokens: u64, output_tokens: u64) -> Self {
        self.history.restore(messages, input_tokens, output_tokens);
        self.persisted = self.history.messages().len();
        // Reuse an existing title (first user message) so resume does not rewrite it.
        self.title = first_user_title(self.history.messages());
        self
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
                    self.persist_turn(&events).await;
                }
                AgentCommand::Shutdown => break,
                // A permission reply or cancel with no turn in flight has nothing to answer.
                AgentCommand::PermissionDecision { .. } | AgentCommand::Cancel => {}
            }
        }
    }

    /// Persist the messages produced by the turn just finished. Runs between turns (never inside the
    /// streaming hot path). A persistence IO error is reported as a non-fatal event and swallowed —
    /// a failed write must not kill the session.
    async fn persist_turn(&mut self, events: &broadcast::Sender<AgentEvent>) {
        let Some(store) = &self.store else {
            return;
        };
        let pending = &self.history.messages()[self.persisted..];
        if let Err(error) = store.append(pending).await {
            let _ = events.send(AgentEvent::Error {
                turn: Some(self.turn),
                message: format!("session persist (append): {error}"),
                fatal: false,
            });
            return;
        }
        self.persisted = self.history.messages().len();
        if self.title.is_none() {
            self.title = first_user_title(self.history.messages());
        }
        let (input_tokens, output_tokens) = self.history.totals();
        let meta = SessionMeta {
            id: store.id().to_string(),
            cwd: self.root.display().to_string(),
            title: self.title.clone(),
            input_tokens,
            output_tokens,
        };
        if let Err(error) = store.write_meta(&meta).await {
            let _ = events.send(AgentEvent::Error {
                turn: Some(self.turn),
                message: format!("session persist (meta): {error}"),
                fatal: false,
            });
        }
    }
}

/// Derive a session title from the first user message: its text, trimmed and truncated to
/// [`TITLE_MAX_CHARS`] characters. Returns `None` when there is no textual user message yet.
fn first_user_title(messages: &[Message]) -> Option<String> {
    use liter_llm::{UserContent, UserMessage};
    messages.iter().find_map(|message| match message {
        Message::User(UserMessage {
            content: UserContent::Text(text),
            ..
        }) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            Some(trimmed.chars().take(TITLE_MAX_CHARS).collect())
        }
        _ => None,
    })
}
