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

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::transport::MAX_FRAME_BYTES;

/// Map a msgpack encode error to an `io::Error` so the handshake helpers return one error type.
fn encode_io<T: serde::Serialize>(msg: &T) -> std::io::Result<Vec<u8>> {
    encode(msg).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Map a msgpack decode error to an `io::Error` so the handshake helpers return one error type.
fn decode_io<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> std::io::Result<T> {
    decode(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write one length-delimited handshake frame: a `u32` big-endian byte length followed by the
/// msgpack body. Matches the [`LengthDelimitedCodec`](tokio_util::codec::LengthDelimitedCodec)
/// wire shape the legacy comms link uses, but written by hand so the reader consumes EXACTLY the
/// handshake frame and leaves the following raw rmcp bytes untouched (a `Framed` reader would
/// buffer past the frame boundary and swallow them). Rejects an over-[`MAX_FRAME_BYTES`] body.
async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, body: &[u8]) -> std::io::Result<()> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("relay frame {} exceeds MAX_FRAME_BYTES {MAX_FRAME_BYTES}", body.len()),
        ));
    }
    let len = u32::try_from(body.len()).expect("len <= MAX_FRAME_BYTES fits u32");
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one length-delimited handshake frame written by [`write_frame`]. Reads the `u32` length,
/// rejects an over-[`MAX_FRAME_BYTES`] prefix (the same defensive cap the legacy codec applies, and
/// what makes an older daemon reject the [`RELAY_MAGIC`] preamble), then reads exactly that many
/// body bytes — no more, so the stream is left positioned at the first post-handshake byte.
async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes).await?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("relay frame length {len} exceeds MAX_FRAME_BYTES {MAX_FRAME_BYTES}"),
        ));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

/// Client side of the relay handshake: write the [`RELAY_MAGIC`] preamble, send `hello`, and read
/// the daemon's [`RelayWelcome`]. Leaves the stream positioned at the first post-handshake byte so
/// the caller can immediately hand it to rmcp when `welcome.accepted`. Any I/O or decode failure
/// (including an older daemon that rejected the preamble as an over-long frame) surfaces as an
/// `io::Error`, on which the caller falls back to an in-process serve.
pub async fn client_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    hello: &RelayHello,
) -> std::io::Result<RelayWelcome> {
    stream.write_all(&RELAY_MAGIC).await?;
    write_frame(stream, &encode_io(hello)?).await?;
    let body = read_frame(stream).await?;
    decode_io(&body)
}

/// Daemon side, phase 1 — read the [`RelayHello`] frame. Called by the accept loop AFTER it has
/// already consumed the 8-byte [`RELAY_MAGIC`] preamble it peeked to route the connection here.
pub async fn read_hello<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<RelayHello> {
    let body = read_frame(reader).await?;
    decode_io(&body)
}

/// Daemon side, phase 1 — send the [`RelayWelcome`] reply. After this the daemon drops the frame
/// codec and hands the raw stream to rmcp (when `welcome.accepted`).
pub async fn write_welcome<W: AsyncWrite + Unpin>(writer: &mut W, welcome: &RelayWelcome) -> std::io::Result<()> {
    write_frame(writer, &encode_io(welcome)?).await
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

    /// Drive both ends of the handshake over an in-memory duplex: the client writes MAGIC + hello,
    /// the server consumes the 8 magic bytes (as the accept loop would after a peek), reads the
    /// hello, replies with a welcome, and the client decodes it. A trailing raw byte written after
    /// the welcome must survive untouched — proving the framed reader stops exactly at the frame
    /// boundary and does not swallow the following rmcp stream.
    #[tokio::test]
    async fn handshake_round_trips_and_leaves_stream_at_first_rmcp_byte() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut client, mut server) = tokio::io::duplex(4096);
        let hello = RelayHello {
            relay_proto_ver: RELAY_PROTO_VER,
            root: std::path::PathBuf::from("/repo/root"),
            view: "main".to_string(),
            agent: AgentId::parse("claude-code").expect("agent"),
        };
        let welcome = RelayWelcome {
            relay_proto_ver: RELAY_PROTO_VER,
            daemon_version: "9.9.9".to_string(),
            accepted: true,
            code: None,
        };

        let server_hello = hello.clone();
        let server_welcome = welcome.clone();
        let server_task = tokio::spawn(async move {
            let mut magic = [0u8; RELAY_MAGIC.len()];
            server.read_exact(&mut magic).await.expect("read magic");
            assert_eq!(magic, RELAY_MAGIC);
            let got = read_hello(&mut server).await.expect("read hello");
            assert_eq!(got, server_hello);
            write_welcome(&mut server, &server_welcome)
                .await
                .expect("write welcome");
            server.write_all(b"{").await.expect("write first rmcp byte");
            server.flush().await.expect("flush");
        });

        let got_welcome = client_handshake(&mut client, &hello).await.expect("handshake");
        assert_eq!(got_welcome, welcome);
        let mut first = [0u8; 1];
        client.read_exact(&mut first).await.expect("read first rmcp byte");
        assert_eq!(
            &first, b"{",
            "the byte after the welcome must be the untouched rmcp stream"
        );
        server_task.await.expect("server task");
    }
}
