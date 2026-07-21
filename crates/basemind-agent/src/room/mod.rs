//! The multi-agent room: a UI-agnostic boundary onto basemind's comms broker.
//!
//! [`RoomClient`] mirrors the [`AgentClient`](crate::transport::AgentClient) seam. It has two
//! sides: a request side ([`roster`](RoomClient::roster) / [`post`](RoomClient::post) /
//! [`history`](RoomClient::history)) the engine and tools call, and an incoming side
//! ([`spawn_incoming`](RoomClient::spawn_incoming)) that pushes peer messages straight onto the
//! engine's existing [`AgentEvent`] broadcast — so surfacing a roster or an incoming message needs
//! no change to the runner's command loop, and the event serializes over any transport for free.
//!
//! Two implementations sit behind the trait: a real one over
//! [`CommsClient`](basemind::comms::CommsClient) (feature `comms`) and a scripted test double
//! (feature `test-util`), so no PTY or unit test ever needs a live broker.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::error::Result;
use crate::event::AgentEvent;

/// A peer agent visible in the room roster.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomPeer {
    /// The peer's stable agent id.
    pub id: String,
    /// A human-facing display name (falls back to `id` when the peer has no card name).
    pub display: String,
}

/// A message posted to the room by a peer (or echoed for the local agent's own post).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomMessage {
    /// The posting agent's id.
    pub from: String,
    /// The message subject (front-matter).
    pub subject: String,
    /// The message body.
    pub body: String,
}

/// The engine's view of the room: post/read on demand, and a background task that streams incoming
/// peer messages onto the event broadcast. Held as `Arc<dyn RoomClient>` so the concrete transport
/// (real broker vs scripted) is chosen at construction and never leaks into the engine or the UI.
#[async_trait]
pub trait RoomClient: Send + Sync + 'static {
    /// The current roster of peer agents.
    async fn roster(&self) -> Result<Vec<RoomPeer>>;

    /// Post a message to the room. `subject` defaults to a short derived line when `None`.
    async fn post(&self, subject: Option<String>, text: String) -> Result<()>;

    /// Room history since `since_micros` (all history when `None`), oldest first.
    async fn history(&self, since_micros: Option<i64>) -> Result<Vec<RoomMessage>>;

    /// Spawn the background task that emits [`AgentEvent::RoomRoster`] once and an
    /// [`AgentEvent::RoomMessage`] per incoming peer message onto `events`. Fire-and-forget: the
    /// task exits when the broadcast closes (the session ended). Called once at session start.
    fn spawn_incoming(&self, events: broadcast::Sender<AgentEvent>);
}
