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

    /// Leave the room (best-effort). The default is a no-op, for transports with no explicit leave;
    /// the broker-backed room releases its thread membership.
    async fn leave(&self) -> Result<()> {
        Ok(())
    }
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

/// One entry on a [`ScriptedRoom`] presence timeline: a peer joining or leaving after a delay. Played
/// out as [`AgentEvent::RoomPeerJoined`] / [`AgentEvent::RoomPeerLeft`], interleaved (by wall clock)
/// with the message timeline.
#[cfg(any(test, feature = "test-util"))]
#[derive(Clone, Debug)]
pub enum ScriptedPresence {
    /// A peer joins the room after the delay.
    Joined {
        /// The joining peer.
        peer: RoomPeer,
        /// How long the presence task sleeps before announcing the join.
        after: std::time::Duration,
    },
    /// A peer (identified by id) leaves the room after the delay.
    Left {
        /// The departing peer's id.
        id: String,
        /// How long the presence task sleeps before announcing the departure.
        after: std::time::Duration,
    },
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
    presence: Vec<ScriptedPresence>,
    posts: PostLog,
    left: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(any(test, feature = "test-util"))]
impl ScriptedRoom {
    /// Build a scripted room from a static roster and an incoming timeline. No presence deltas until
    /// [`with_presence`](Self::with_presence) attaches a timeline.
    pub fn new(roster: Vec<RoomPeer>, incoming: Vec<ScriptedIncoming>) -> Self {
        Self {
            roster,
            incoming,
            presence: Vec::new(),
            posts: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            left: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Attach a presence timeline of scripted joins/leaves, played out alongside the incoming feed.
    pub fn with_presence(mut self, presence: Vec<ScriptedPresence>) -> Self {
        self.presence = presence;
        self
    }

    /// A snapshot of every `(subject, text)` passed to [`post`](RoomClient::post), oldest first.
    pub fn posts(&self) -> Vec<(Option<String>, String)> {
        self.posts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Whether [`leave`](RoomClient::leave) has been called (shared across clones).
    pub fn left(&self) -> bool {
        self.left.load(std::sync::atomic::Ordering::SeqCst)
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
        let messages = events.clone();
        tokio::spawn(async move {
            // A send error means the broadcast closed (session ended) — stop the timeline. ~keep
            if messages.send(AgentEvent::RoomRoster { peers: roster }).is_err() {
                return;
            }
            for entry in incoming {
                tokio::time::sleep(entry.after).await;
                if messages.send(AgentEvent::RoomMessage(entry.message)).is_err() {
                    return;
                }
            }
        });
        // Presence deltas run on their own task so their delays interleave (by wall clock) with the ~keep
        // message feed above without either serializing behind the other. ~keep
        let presence = self.presence.clone();
        tokio::spawn(async move {
            for entry in presence {
                let (after, event) = match entry {
                    ScriptedPresence::Joined { peer, after } => (after, AgentEvent::RoomPeerJoined { peer }),
                    ScriptedPresence::Left { id, after } => (after, AgentEvent::RoomPeerLeft { id }),
                };
                tokio::time::sleep(after).await;
                if events.send(event).is_err() {
                    return;
                }
            }
        });
    }

    async fn leave(&self) -> Result<()> {
        self.left.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}
