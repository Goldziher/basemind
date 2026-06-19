//! Wire protocol between comms clients (serve / CLI / hooks) and the broker daemon.
//!
//! JSON-RPC-shaped: [`CommsRequest`] is an internally-tagged `method` + `params` enum and
//! [`CommsResponse`] / [`CommsNotification`] mirror it, so a future A2A HTTP front-end can
//! serialize the SAME enums to JSON and reuse this contract verbatim. Over the local IPC
//! transport the bodies are msgpack, but the serde shape is transport-agnostic.
//!
//! `proto_ver` negotiation in [`CommsRequest::Hello`] guards version skew: the daemon rejects
//! a client whose major protocol version it does not speak rather than silently
//! mis-deserializing a future request shape.

use serde::{Deserialize, Serialize};

use super::cursor::Cursor;
use super::ids::{AgentId, RoomId};
use super::model::{AgentCard, MessageMeta, Room, RoomScope};

/// The protocol version this build speaks. Bumped on any breaking change to the request /
/// response / notification shapes. Negotiated in [`CommsRequest::Hello`].
pub const PROTO_VER: u32 = 1;

/// A request from a client to the broker. `method` selects the variant; `params` are the
/// flattened fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum CommsRequest {
    /// First frame on a link: announce identity and negotiate protocol version. Also carries
    /// the optional scope context (remote + cwd) so the broker can auto-join scoped rooms.
    Hello {
        /// The connecting agent's id.
        agent: AgentId,
        /// The protocol version the client speaks.
        proto_ver: u32,
        /// Normalised git remote of the agent's repo, if any.
        #[serde(default)]
        remote: Option<String>,
        /// The agent's current working directory, if it wishes to auto-join path rooms.
        #[serde(default)]
        cwd: Option<std::path::PathBuf>,
    },
    /// Register or update the agent's card.
    Register {
        /// The agent's self-described A2A card.
        card: AgentCard,
    },
    /// List known agents, optionally scoped to the subscribers of one room.
    ListAgents {
        /// Restrict to subscribers of this room when set.
        #[serde(default)]
        room: Option<RoomId>,
    },
    /// Create (and register) a room with an explicit scope.
    CreateRoom {
        /// The room id to create.
        room: RoomId,
        /// The scope governing auto-join.
        scope: RoomScope,
        /// Optional human title.
        #[serde(default)]
        title: Option<String>,
    },
    /// List rooms whose scope matches the supplied scope chain.
    ListRooms {
        /// Remote of the agent's repo, if any.
        #[serde(default)]
        remote: Option<String>,
        /// The agent's cwd, used for path-prefix matching.
        #[serde(default)]
        cwd: Option<std::path::PathBuf>,
    },
    /// Subscribe the calling agent to a room (and start receiving notifications).
    Join {
        /// The room to join.
        room: RoomId,
    },
    /// Unsubscribe the calling agent from a room.
    Leave {
        /// The room to leave.
        room: RoomId,
    },
    /// Post a message to a room. Returns the new message id.
    Post {
        /// Target room.
        room: RoomId,
        /// Subject line.
        subject: String,
        /// Free-form tags.
        #[serde(default)]
        tags: Vec<String>,
        /// Id of the message being replied to, for threading.
        #[serde(default)]
        reply_to: Option<String>,
        /// The message body bytes.
        body: Vec<u8>,
    },
    /// Read a room's history, oldest-first, paginated. Returns front-matter only.
    History {
        /// The room to read.
        room: RoomId,
        /// Resume token from a previous page.
        #[serde(default)]
        cursor: Option<Cursor>,
        /// Maximum messages to return.
        #[serde(default)]
        limit: Option<u32>,
    },
    /// Fetch a single message's body by id.
    GetBody {
        /// The message id (the `id` field of a [`MessageMeta`]).
        message_id: String,
    },
    /// Read the calling agent's inbox: new messages across all subscribed rooms.
    Inbox {
        /// Remote of the agent's repo, for auto-join before the read.
        #[serde(default)]
        remote: Option<String>,
        /// The agent's cwd, for auto-join before the read.
        #[serde(default)]
        cwd: Option<std::path::PathBuf>,
        /// Resume token from a previous page.
        #[serde(default)]
        cursor: Option<Cursor>,
        /// Maximum messages to return.
        #[serde(default)]
        limit: Option<u32>,
        /// When true, advance the agent's read cursors past the returned messages.
        #[serde(default)]
        mark_read: bool,
    },
    /// Open a notification stream for a room (the link receives [`CommsNotification::Message`]
    /// for every subsequent post). Returns a subscription handle.
    Subscribe {
        /// The room to stream.
        room: RoomId,
    },
    /// Cancel a notification stream opened by [`CommsRequest::Subscribe`].
    Unsubscribe {
        /// The subscription handle returned by `Subscribe`.
        sub: u64,
    },
    /// Liveness probe. The daemon replies [`CommsResponse::Pong`].
    Ping,
    /// Ask the daemon to drain and stop. Used by `basemind comms stop`.
    Stop,
    /// Report daemon status (pid / version / uptime / room + subscriber counts).
    Status,
}

/// A response from the broker to a [`CommsRequest`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", content = "data", rename_all = "snake_case")]
pub enum CommsResponse {
    /// Reply to [`CommsRequest::Hello`]: the daemon's protocol version + accept/reject.
    Welcome {
        /// The protocol version the daemon speaks.
        proto_ver: u32,
        /// Daemon build version string.
        daemon_version: String,
    },
    /// Acknowledge a side-effecting request that returns no payload.
    Ok,
    /// Reply to [`CommsRequest::ListAgents`].
    Agents(Vec<super::model::AgentRecord>),
    /// Reply to [`CommsRequest::CreateRoom`] and room lookups.
    Room(Room),
    /// Reply to [`CommsRequest::ListRooms`].
    Rooms(Vec<Room>),
    /// Reply to [`CommsRequest::Post`]: the new message id.
    Posted {
        /// The id of the message just stored.
        message_id: String,
    },
    /// Reply to [`CommsRequest::History`].
    History {
        /// The page of front-matter records.
        messages: Vec<MessageMeta>,
        /// Resume token for the next page, when more remain.
        next_cursor: Option<Cursor>,
    },
    /// Reply to [`CommsRequest::Inbox`].
    Inbox {
        /// The page of front-matter records across subscribed rooms.
        messages: Vec<MessageMeta>,
        /// Count of unread messages remaining after this page.
        unread: u32,
        /// Resume token for the next page, when more remain.
        next_cursor: Option<Cursor>,
    },
    /// Reply to [`CommsRequest::GetBody`].
    Body {
        /// The body bytes, or `None` when the message id is unknown.
        body: Option<Vec<u8>>,
    },
    /// Reply to [`CommsRequest::Subscribe`]: the subscription handle.
    Subscribed {
        /// The handle to pass to [`CommsRequest::Unsubscribe`].
        sub: u64,
    },
    /// Reply to [`CommsRequest::Ping`].
    Pong,
    /// Reply to [`CommsRequest::Status`].
    Status(StatusReport),
    /// A request failed. `code` is a stable machine token; `message` is human detail.
    Error {
        /// Stable error token (e.g. `proto_skew`, `unknown_room`, `peer_denied`).
        code: String,
        /// Human-readable detail.
        message: String,
    },
}

/// Daemon status snapshot returned by [`CommsRequest::Status`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// The daemon process id.
    pub pid: u32,
    /// Daemon build version.
    pub version: String,
    /// Protocol version spoken.
    pub proto_ver: u32,
    /// Seconds since the daemon started.
    pub uptime_secs: u64,
    /// Number of registered rooms.
    pub rooms: u32,
    /// Number of live notification subscribers.
    pub subscribers: u32,
}

/// An unsolicited message the broker pushes to a subscribed link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "notify", content = "data", rename_all = "snake_case")]
pub enum CommsNotification {
    /// A new message landed in a room this link subscribes to. Carries front-matter only;
    /// fetch the body via [`CommsRequest::GetBody`].
    Message(MessageMeta),
    /// The daemon is shutting down; the link should disconnect.
    Shutdown,
}

/// A frame sent from broker → client: either a direct response to a request or an
/// out-of-band notification. Both ride the same link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommsOut {
    /// A reply to a specific request.
    Response(CommsResponse),
    /// An out-of-band push.
    Notification(CommsNotification),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_through_msgpack() {
        let req = CommsRequest::Post {
            room: RoomId::parse("room-1").expect("room"),
            subject: "hi".to_string(),
            tags: vec!["t".to_string()],
            reply_to: None,
            body: b"hello".to_vec(),
        };
        let bytes = rmp_serde::to_vec_named(&req).expect("encode");
        let back: CommsRequest = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(req, back);
    }

    #[test]
    fn request_is_json_rpc_shaped() {
        // The `method` tag is what a future A2A HTTP front-end keys on.
        let req = CommsRequest::Ping;
        let json = serde_json::to_value(&req).expect("json");
        assert_eq!(json["method"], "ping");
    }

    #[test]
    fn out_frame_round_trips() {
        let out = CommsOut::Notification(CommsNotification::Shutdown);
        let bytes = rmp_serde::to_vec_named(&out).expect("encode");
        let back: CommsOut = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(out, back);
    }
}
