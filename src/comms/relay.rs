//! Wire types for the stdio↔daemon MCP relay handshake.
//!
//! A relay client (an `rmcp` stdio front-end) can forward its JSON-RPC session to the broker
//! daemon instead of serving in-process. Because a single daemon socket already carries the
//! legacy msgpack comms protocol ([`super::protocol`]), the relay client announces itself with a
//! fixed [`RELAY_MAGIC`] preamble so the daemon's accept loop can `MSG_PEEK` the first bytes and
//! route a relay connection apart from a legacy comms link.
//!
//! After the preamble the client sends a [`RelayHello`] and the daemon answers with a
//! [`RelayWelcome`]. Both ride the same msgpack encoding as the rest of the wire protocol (via
//! [`encode`] / [`decode`]). [`RELAY_PROTO_VER`] negotiates relay-handshake skew independently of
//! [`super::protocol::PROTO_VER`], so the relay layer can evolve without disturbing the comms
//! protocol version.

use serde::{Deserialize, Serialize};

use super::ids::AgentId;

/// The 8-byte preamble a relay client writes first, before any [`RelayHello`], so the daemon's
/// accept loop can distinguish a relay (rmcp JSON-RPC) connection from a legacy msgpack comms
/// link via `MSG_PEEK`.
///
/// The value is chosen to be self-disambiguating against the legacy framing: a comms link starts
/// with a `u32` big-endian [`LengthDelimitedCodec`](tokio_util::codec::LengthDelimitedCodec)
/// length prefix, and this preamble's first four bytes (`b"BMRE"` = `0x424D5245`, ~1.1 GB)
/// vastly exceed [`MAX_FRAME_BYTES`](super::transport::MAX_FRAME_BYTES) (16 MiB). An OLDER daemon
/// that has never heard of the relay therefore reads the preamble as an over-long frame length,
/// rejects it, and closes the link — a clean auto-fallback signal telling the client to serve
/// in-process. A legacy [`Hello`](super::protocol::CommsRequest::Hello) frame begins with a small
/// length whose high byte is `0x00`, so it can never collide with the `0x42` first byte here.
pub const RELAY_MAGIC: [u8; 8] = *b"BMRELAY1";

/// The relay handshake's own protocol version, negotiated in [`RelayHello`] / [`RelayWelcome`].
///
/// Independent of [`super::protocol::PROTO_VER`]: the relay envelope can change shape without a
/// comms-protocol bump, and vice versa. Bumped on any breaking change to the relay handshake.
pub const RELAY_PROTO_VER: u32 = 1;

/// The relay client's opening message, sent immediately after [`RELAY_MAGIC`].
///
/// Identifies the rmcp session's target workspace (`root` + `view`) and the connecting agent
/// identity so the daemon can bind the forwarded JSON-RPC session to the right hot workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayHello {
    /// The relay-handshake protocol version the client speaks.
    pub relay_proto_ver: u32,
    /// Canonical workspace root (worktree root) the session targets.
    pub root: std::path::PathBuf,
    /// The view name within that workspace.
    pub view: String,
    /// The connecting agent's identity.
    pub agent: AgentId,
}

/// The daemon's reply to a [`RelayHello`].
///
/// When `accepted` is `false`, `code` carries a stable machine token (e.g. `"relay_proto_skew"`)
/// telling the client to fall back to in-process `serve` rather than relaying to this daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayWelcome {
    /// The relay-handshake protocol version the daemon speaks.
    pub relay_proto_ver: u32,
    /// The daemon's build version string.
    pub daemon_version: String,
    /// Whether the daemon accepted the relay session.
    pub accepted: bool,
    /// A stable rejection token when `accepted` is `false`; `None` on acceptance.
    pub code: Option<String>,
}

/// Encode a relay handshake message to msgpack, using named fields to match the rest of the wire
/// protocol.
pub fn encode<T: serde::Serialize>(msg: &T) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    rmp_serde::to_vec_named(msg)
}

/// Decode a relay handshake message from msgpack bytes.
pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, rmp_serde::decode::Error> {
    rmp_serde::from_slice(bytes)
}

#[cfg(test)]
mod tests {
    use super::super::transport::MAX_FRAME_BYTES;
    use super::*;

    #[test]
    fn relay_hello_round_trips_through_msgpack() {
        let hello = RelayHello {
            relay_proto_ver: RELAY_PROTO_VER,
            root: std::path::PathBuf::from("/repo/root"),
            view: "main".to_string(),
            agent: AgentId::parse("claude-code").expect("agent"),
        };
        let bytes = encode(&hello).expect("encode");
        let back: RelayHello = decode(&bytes).expect("decode");
        assert_eq!(hello, back);
    }

    #[test]
    fn relay_welcome_round_trips_through_msgpack() {
        let welcome = RelayWelcome {
            relay_proto_ver: RELAY_PROTO_VER,
            daemon_version: "0.22.6".to_string(),
            accepted: false,
            code: Some("relay_proto_skew".to_string()),
        };
        let bytes = encode(&welcome).expect("encode");
        let back: RelayWelcome = decode(&bytes).expect("decode");
        assert_eq!(welcome, back);
    }

    #[test]
    fn relay_magic_is_eight_bytes() {
        assert_eq!(RELAY_MAGIC.len(), 8);
    }

    #[test]
    fn relay_magic_prefix_exceeds_max_frame_bytes() {
        let prefix = [RELAY_MAGIC[0], RELAY_MAGIC[1], RELAY_MAGIC[2], RELAY_MAGIC[3]];
        let as_len = u32::from_be_bytes(prefix) as usize;
        assert!(
            as_len > MAX_FRAME_BYTES,
            "preamble prefix {as_len} must exceed MAX_FRAME_BYTES {MAX_FRAME_BYTES} for fallback"
        );
    }
}
