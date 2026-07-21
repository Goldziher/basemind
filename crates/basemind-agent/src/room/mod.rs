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

#[cfg(feature = "comms")]
mod comms;
#[cfg(feature = "comms")]
pub use comms::CommsRoom;

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

/// One entry on a [`ScriptedRoom`] timeline: a peer message plus the delay before it is delivered.
#[cfg(any(test, feature = "test-util"))]
#[derive(Clone, Debug)]
pub struct ScriptedIncoming {
    /// The peer message to deliver.
    pub message: RoomMessage,
    /// How long the incoming task sleeps before delivering [`message`](Self::message).
    pub after: std::time::Duration,
}

/// A shared, thread-safe log of `(subject, text)` pairs recorded by [`ScriptedRoom::post`].
#[cfg(any(test, feature = "test-util"))]
type PostLog = std::sync::Arc<std::sync::Mutex<Vec<(Option<String>, String)>>>;

/// A deterministic in-memory [`RoomClient`] test double: a static roster, a scripted incoming
/// timeline, and a post-log. It never touches a real broker or [`CommsClient`](basemind::comms) —
/// available only under test or the `test-util` feature so a PTY suite or unit test can exercise the
/// room seam with no network. Clone shares the post-log via its [`Arc`](std::sync::Arc).
#[cfg(any(test, feature = "test-util"))]
#[derive(Clone)]
pub struct ScriptedRoom {
    roster: Vec<RoomPeer>,
    incoming: Vec<ScriptedIncoming>,
    posts: PostLog,
}

#[cfg(any(test, feature = "test-util"))]
impl ScriptedRoom {
    /// Build a scripted room from a static roster and an incoming timeline.
    pub fn new(roster: Vec<RoomPeer>, incoming: Vec<ScriptedIncoming>) -> Self {
        Self {
            roster,
            incoming,
            posts: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// A snapshot of every `(subject, text)` passed to [`post`](RoomClient::post), oldest first.
    pub fn posts(&self) -> Vec<(Option<String>, String)> {
        self.posts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[cfg(any(test, feature = "test-util"))]
#[async_trait]
impl RoomClient for ScriptedRoom {
    async fn roster(&self) -> Result<Vec<RoomPeer>> {
        Ok(self.roster.clone())
    }

    async fn post(&self, subject: Option<String>, text: String) -> Result<()> {
        // Record only; posts are NOT fed back into the incoming stream — the real broker excludes ~keep
        // self-posts from your own inbox and the UI echoes the human's own post locally. ~keep
        self.posts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((subject, text));
        Ok(())
    }

    async fn history(&self, _since_micros: Option<i64>) -> Result<Vec<RoomMessage>> {
        Ok(self.incoming.iter().map(|entry| entry.message.clone()).collect())
    }

    fn spawn_incoming(&self, events: broadcast::Sender<AgentEvent>) {
        let roster = self.roster.clone();
        let incoming = self.incoming.clone();
        tokio::spawn(async move {
            // A send error means the broadcast closed (session ended) — stop the timeline. ~keep
            if events.send(AgentEvent::RoomRoster { peers: roster }).is_err() {
                return;
            }
            for entry in incoming {
                tokio::time::sleep(entry.after).await;
                if events.send(AgentEvent::RoomMessage(entry.message)).is_err() {
                    return;
                }
            }
        });
    }
}
