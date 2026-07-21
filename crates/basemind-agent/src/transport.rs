//! The UI-agnostic boundary between the engine and a front-end.
//!
//! A UI depends only on [`AgentClient`] — it never names a channel or a socket. Today the sole
//! implementation is [`InProcAgentClient`], backed by tokio channels straight to the engine task
//! in the same process. A future cross-process implementation (msgpack over a Unix socket) can
//! satisfy the same trait with zero UI changes, because [`AgentEvent`]/[`AgentCommand`] are serde.
//! This mirrors basemind's own `CommsLink` seam (`src/comms/transport.rs`), which has an in-proc
//! mpsc impl and a UDS-msgpack impl behind one trait.

use std::future::Future;
use std::io;

use tokio::sync::{broadcast, mpsc};

use crate::command::AgentCommand;
use crate::event::AgentEvent;

/// The front-end's view of a running agent: a stream of events and a command sink. UIs are
/// written generic over `impl AgentClient`, so swapping the in-process impl for a cross-process
/// one later requires no UI change.
pub trait AgentClient: Send + 'static {
    /// Await the next event, or `None` once the engine has shut down.
    fn next_event(&mut self) -> impl Future<Output = Option<AgentEvent>> + Send;

    /// Send a command to the engine. Errors only if the engine is gone.
    fn send_command(&self, command: AgentCommand) -> impl Future<Output = io::Result<()>> + Send;
}

/// The engine's half of an in-process connection: it receives commands and broadcasts events.
pub struct EngineEndpoint {
    /// Commands from the UI.
    pub commands: mpsc::Receiver<AgentCommand>,
    /// Event sink broadcast to all subscribers (UI, logger, ...).
    pub events: broadcast::Sender<AgentEvent>,
}

/// The UI's half of an in-process connection.
pub struct InProcAgentClient {
    commands: mpsc::Sender<AgentCommand>,
    events: broadcast::Receiver<AgentEvent>,
}

impl InProcAgentClient {
    /// Subscribe another receiver to the same event stream (e.g. a logger alongside the UI).
    pub fn resubscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.events.resubscribe()
    }

    /// Mint an independent front-end client for the same engine: a cloned command sink plus a fresh
    /// event subscription. Every minted client can send commands and sees the same event stream, so
    /// one long-lived engine can serve a sequence of front-ends (a daemon accepting reconnects) or
    /// several at once. The engine stays alive as long as any client — including the template this
    /// was minted from — holds the command sink.
    pub fn new_client(&self) -> InProcAgentClient {
        InProcAgentClient {
            commands: self.commands.clone(),
            events: self.events.resubscribe(),
        }
    }
}

/// Create a connected engine/UI pair. `command_buffer` bounds queued commands; `event_buffer`
/// bounds the broadcast backlog before a slow subscriber starts lagging.
pub fn in_proc_channel(command_buffer: usize, event_buffer: usize) -> (EngineEndpoint, InProcAgentClient) {
    let (cmd_tx, cmd_rx) = mpsc::channel(command_buffer);
    let (evt_tx, evt_rx) = broadcast::channel(event_buffer);
    (
        EngineEndpoint {
            commands: cmd_rx,
            events: evt_tx,
        },
        InProcAgentClient {
            commands: cmd_tx,
            events: evt_rx,
        },
    )
}

impl AgentClient for InProcAgentClient {
    async fn next_event(&mut self) -> Option<AgentEvent> {
        loop {
            match self.events.recv().await {
                Ok(event) => return Some(event),
                Err(broadcast::error::RecvError::Closed) => return None,
                // A slow UI fell behind; skip the gap and keep going rather than dying. ~keep
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    }

    async fn send_command(&self, command: AgentCommand) -> io::Result<()> {
        self.commands
            .send(command)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "agent engine is gone"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentEvent;

    #[tokio::test]
    async fn new_client_shares_the_engine_event_stream_and_command_sink() {
        let (mut endpoint, client) = in_proc_channel(4, 8);
        // Mint two independent front-ends before broadcasting, so their subscriptions see it. ~keep
        let mut first = client.new_client();
        let mut second = client.new_client();

        endpoint
            .events
            .send(AgentEvent::TurnStarted { turn: 7 })
            .expect("broadcast reaches subscribers");
        assert_eq!(first.next_event().await, Some(AgentEvent::TurnStarted { turn: 7 }));
        assert_eq!(second.next_event().await, Some(AgentEvent::TurnStarted { turn: 7 }));

        first
            .send_command(AgentCommand::Cancel)
            .await
            .expect("command reaches engine");
        assert_eq!(endpoint.commands.recv().await, Some(AgentCommand::Cancel));

        // Dropping the template and one client leaves the engine reachable via the survivor. ~keep
        drop(client);
        drop(second);
        first
            .send_command(AgentCommand::Shutdown)
            .await
            .expect("engine still reachable through the surviving client");
        assert_eq!(endpoint.commands.recv().await, Some(AgentCommand::Shutdown));
    }
}
