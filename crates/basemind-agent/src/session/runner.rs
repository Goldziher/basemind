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
use crate::room::RoomClient;
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
    room: Option<Arc<dyn RoomClient>>,
    room_auto_respond: bool,
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
            room: None,
            room_auto_respond: false,
        })
    }

    /// Build a session from an already-resolved provider pool instead of live config. Available only
    /// under test / the `test-util` feature: it drives the real runner (streaming, permission,
    /// cancel, persistence) with a scripted [`ModelClient`](crate::model::ModelClient), so a
    /// controlled smoke exercises the same code path as the TUI with no network and no API keys.
    #[cfg(any(test, feature = "test-util"))]
    pub fn with_provider(
        provider: ProviderPool,
        root: PathBuf,
        server: Option<Arc<BasemindServer>>,
        tools: ToolRegistry,
        system_prompt: Option<String>,
        max_steps: u32,
    ) -> Self {
        Self {
            history: History::new(system_prompt),
            tools,
            provider,
            permission: PermissionEngine::with_base(),
            root,
            server,
            role: Role::Default,
            max_steps,
            turn: 0,
            store: None,
            persisted: 0,
            title: None,
            room: None,
            room_auto_respond: false,
        }
    }

    /// Persist this session's turns to `store`, appending new messages between turns.
    pub fn persist_to(mut self, store: SessionStore) -> Self {
        self.store = Some(store);
        self
    }

    /// Wire a multi-agent room: its incoming peer messages stream onto the event broadcast and
    /// `RoomPost` commands are forwarded to it. Absent a room, both stay no-ops.
    pub fn with_room(mut self, room: Arc<dyn RoomClient>) -> Self {
        self.room = Some(room);
        self
    }

    /// Opt into auto-responding to the room: while idle, an incoming peer message starts a turn so
    /// the agent can react (e.g. reply via `room:post`). Off by default — wiring a room only surfaces
    /// messages; it does not make the agent chatty. No effect without a room.
    pub fn with_room_auto_respond(mut self, on: bool) -> Self {
        self.room_auto_respond = on;
        self
    }

    /// Seed the history from a resumed session: append `messages`, set the token totals, and mark
    /// them already-persisted so they are not re-appended on the next turn.
    pub fn seed(mut self, messages: Vec<Message>, input_tokens: u64, output_tokens: u64) -> Self {
        self.history.restore(messages, input_tokens, output_tokens);
        self.persisted = self.history.messages().len();
        // Reuse an existing title (first user message) so resume does not rewrite it. ~keep
        self.title = first_user_title(self.history.messages());
        self
    }

    /// Run until the command channel closes or a `Shutdown` arrives. Each `UserMessage` drives one
    /// turn; permission replies are consumed inside the turn. With room auto-respond on, an incoming
    /// peer message received while idle also drives a turn.
    pub async fn run(mut self, endpoint: EngineEndpoint) {
        let EngineEndpoint { mut commands, events } = endpoint;
        // Incoming room messages ride the existing event broadcast, so surfacing a roster or a peer
        // message needs no command-loop change. ~keep
        if let Some(room) = &self.room {
            room.spawn_incoming(events.clone());
        }
        // For auto-respond we watch that same broadcast for `RoomMessage`s, so a peer message can
        // start a turn while idle. A second subscriber never perturbs the primary UI stream. ~keep
        let mut wake = (self.room.is_some() && self.room_auto_respond).then(|| events.subscribe());
        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        AgentCommand::UserMessage { text } => {
                            self.run_user_turn(text, &events, &mut commands).await;
                        }
                        AgentCommand::Shutdown => break,
                        AgentCommand::RoomPost { subject, text } => {
                            if let Some(room) = &self.room
                                && let Err(error) = room.post(subject, text).await
                            {
                                let _ = events.send(AgentEvent::Error {
                                    turn: None,
                                    message: format!("room post: {error}"),
                                    fatal: false,
                                });
                            }
                        }
                        // A permission reply or cancel with no turn in flight has nothing to answer;
                        // a room post with no room wired stays a no-op. ~keep
                        AgentCommand::PermissionDecision { .. } | AgentCommand::Cancel => {}
                    }
                }
                message = recv_room_message(wake.as_mut()), if wake.is_some() => match message {
                    // Frame the peer message as the turn's user input so the agent can react to it. ~keep
                    Some(message) => {
                        let text = format!("[room] {}: {}", message.from, message.body);
                        self.run_user_turn(text, &events, &mut commands).await;
                    }
                    // The broadcast closed; stop watching for wakes. ~keep
                    None => wake = None,
                },
            }
        }
    }

    /// Push `text` as a user message and drive one turn to completion, then persist it. Shared by the
    /// command path and the room-auto-respond wake path.
    async fn run_user_turn(
        &mut self,
        text: String,
        events: &broadcast::Sender<AgentEvent>,
        commands: &mut tokio::sync::mpsc::Receiver<AgentCommand>,
    ) {
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
            room: self.room.clone(),
            max_steps: self.max_steps,
        };
        run_turn(self.turn, &mut cx, events, commands).await;
        self.persist_turn(events).await;
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

/// Await the next incoming [`RoomMessage`](crate::room::RoomMessage) on the wake receiver, draining
/// past every other event. Returns `None` once the broadcast closes. When `rx` is `None` it parks
/// forever, so the enclosing `select!` must guard this branch on `wake.is_some()`.
async fn recv_room_message(rx: Option<&mut broadcast::Receiver<AgentEvent>>) -> Option<crate::room::RoomMessage> {
    let Some(rx) = rx else {
        // Never resolve: the caller's `if wake.is_some()` guard keeps us out of this arm. ~keep
        std::future::pending::<()>().await;
        return None;
    };
    loop {
        match rx.recv().await {
            Ok(AgentEvent::RoomMessage(message)) => return Some(message),
            // Any other event — or a lagged gap under a busy turn — is irrelevant to waking. ~keep
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return None,
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
