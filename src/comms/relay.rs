//! Wire types for the stdio↔daemon MCP relay handshake.
//!
//! A relay client (an `rmcp` stdio front-end) forwards its JSON-RPC session to the broker
//! daemon; `basemind serve` has no in-process server. Because a single daemon socket already carries the
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

use super::v2::api::Principal;

/// The 8-byte preamble a relay client writes first, before any [`RelayHello`], so the daemon's
/// accept loop can distinguish a relay (rmcp JSON-RPC) connection from a legacy msgpack comms
/// link via `MSG_PEEK`.
///
/// The value is chosen to be self-disambiguating against the legacy framing: a comms link starts
/// with a `u32` big-endian [`LengthDelimitedCodec`](tokio_util::codec::LengthDelimitedCodec)
/// length prefix, and this preamble's first four bytes (`b"BMRE"` = `0x424D5245`, ~1.1 GB)
/// vastly exceed [`MAX_FRAME_BYTES`](super::transport::MAX_FRAME_BYTES) (16 MiB). An OLDER daemon
/// that has never heard of the relay therefore reads the preamble as an over-long frame length,
/// rejects it, and closes the link, which the client sees as a failed handshake. A legacy [`Hello`](super::protocol::CommsRequest::Hello) frame begins with a small
/// length whose high byte is `0x00`, so it can never collide with the `0x42` first byte here.
pub const RELAY_MAGIC: [u8; 8] = *b"BMRELAY1";

/// How long the daemon waits for a relay link's preamble and [`RelayHello`]. A link counts as a
/// session being set up from its accept, which makes the daemon refuse `Stop`; one that never
/// says hello must not do that for good.
pub const HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Largest piece of a relay frame's body allocated before it has arrived.
const FRAME_READ_CHUNK: usize = 64 * 1024;

/// The relay handshake's own protocol version, negotiated in [`RelayHello`] / [`RelayWelcome`].
///
/// Independent of [`super::protocol::PROTO_VER`]: the relay envelope can change shape without a
/// comms-protocol bump, and vice versa. Bumped on any breaking change to the relay handshake: 1→2
/// when [`RelayHello`] began naming the session's principal and host (ADR-0021) in place of a
/// per-connection agent id.
pub const RELAY_PROTO_VER: u32 = 2;

/// The relay client's opening message, sent immediately after [`RELAY_MAGIC`].
///
/// Identifies the rmcp session's target workspace (`root` + `view`) and who the session is, so the
/// daemon can bind the forwarded JSON-RPC session to the right hot workspace and serve its agent
/// tools as that principal (ADR-0021).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayHello {
    /// The relay-handshake protocol version the client speaks.
    pub relay_proto_ver: u32,
    /// Canonical workspace root (worktree root) the session targets.
    pub root: std::path::PathBuf,
    /// The view name within that workspace.
    pub view: String,
    /// The principal the session resolved, through the resolver the CLI and the hooks use.
    pub principal: Principal,
    /// The agent harness (`BASEMIND_HOST`).
    #[serde(default)]
    pub host: Option<String>,
    /// What the session's first agent-tool reply tells the agent about its identity: a
    /// configured id that was refused.
    #[serde(default)]
    pub notices: Vec<String>,
}

/// The daemon's reply to a [`RelayHello`].
///
/// When `accepted` is `false`, `code` carries a stable machine token (e.g. `"relay_proto_skew"`)
/// from which the client decides whether to retry, and how soon. There is no in-process `serve`
/// to fall back to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayWelcome {
    /// The relay-handshake protocol version the daemon speaks.
    pub relay_proto_ver: u32,
    /// The daemon's build version string.
    pub daemon_version: String,
    /// Whether the daemon accepted the relay session.
    pub accepted: bool,
    /// A stable rejection token when `accepted` is `false`; `None` on acceptance. One of the
    /// [`decline`] tokens; a client treats a token it does not know as a refusal that may clear.
    pub code: Option<String>,
    /// What the token alone cannot say, for the person reading the refusal: who holds the
    /// workspace, or why the root was refused. Absent on acceptance and from a daemon that has
    /// nothing to add.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The tokens a daemon declines a relay session with ([`RelayWelcome::code`]).
pub mod decline {
    /// The client speaks another relay protocol version.
    pub const PROTO_SKEW: &str = "relay_proto_skew";
    /// The session asked for a view the daemon does not host. The same on every retry.
    pub const VIEW_UNSUPPORTED: &str = "view_unsupported";
    /// The root is not a directory basemind indexes. The same on every retry.
    pub const ROOT_REFUSED: &str = "root_refused";
    /// The daemon accepted a `Stop` and is on its way out; its successor accepts the session.
    pub const DAEMON_DRAINING: &str = "daemon_draining";
    /// Another basemind process holds the workspace's index lock. Clears when that process lets go.
    pub const WORKSPACE_LOCKED: &str = "workspace_locked";
    /// The daemon's own scan of the workspace holds its store. Clears within moments.
    pub const WORKSPACE_BUSY: &str = "workspace_busy";
    /// The workspace's index was written by a newer basemind than this daemon. The same on every
    /// retry, until this basemind is updated.
    pub const INDEX_NEWER: &str = "index_newer";
    /// The daemon could not set the workspace up for a reason it has no token for.
    pub const HOST_BUILD_FAILED: &str = "host_build_failed";
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
    // Grow with what arrives rather than allocating the declared length up front: a peer could
    // otherwise make the daemon allocate 16 MiB per link before sending a byte of body. ~keep
    let mut body = Vec::with_capacity(len.min(FRAME_READ_CHUNK));
    let read = (&mut *reader).take(len as u64).read_to_end(&mut body).await?;
    if read < len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("relay frame ended after {read} of {len} bytes"),
        ));
    }
    Ok(body)
}

/// Client side of the relay handshake: write the [`RELAY_MAGIC`] preamble, send `hello`, and read
/// the daemon's [`RelayWelcome`]. Leaves the stream positioned at the first post-handshake byte so
/// the caller can immediately hand it to rmcp when `welcome.accepted`. Any I/O or decode failure
/// (including an older daemon that rejected the preamble as an over-long frame) surfaces as an
/// `io::Error`, which the caller retries as a failed connection.
pub async fn client_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    hello: &RelayHello,
) -> std::io::Result<RelayWelcome> {
    stream.write_all(&RELAY_MAGIC).await?;
    write_frame(stream, &encode_io(hello)?).await?;
    let body = read_frame(stream).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            // ~keep A daemon of relay protocol 1 cannot read this hello and closes the link
            // ~keep without a welcome; one of protocol 2 or later declines a skew in a welcome.
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "the daemon closed the link without answering this relay's hello (relay protocol \
                     {RELAY_PROTO_VER}); it is most likely an older basemind that cannot read it"
                ),
            )
        } else {
            error
        }
    })?;
    decode_io(&body)
}

/// The one field every relay hello carries, read from a hello whose other fields are another
/// version's.
#[derive(Deserialize)]
struct HelloVersion {
    relay_proto_ver: u32,
}

/// A relay hello written for another relay protocol version, whose fields this binary cannot read:
/// the daemon answers it with [`decline::PROTO_SKEW`] instead of dropping the link.
#[derive(Debug)]
pub struct SkewedHello {
    /// The version the client speaks.
    pub relay_proto_ver: u32,
}

impl std::fmt::Display for SkewedHello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "relay hello of protocol {}, this daemon speaks {RELAY_PROTO_VER}",
            self.relay_proto_ver
        )
    }
}

impl std::error::Error for SkewedHello {}

/// Decode a [`RelayHello`]. A body that does not decode but names another protocol version is a
/// [`SkewedHello`] inside the `InvalidData` error, for the caller to decline.
fn decode_hello(body: &[u8]) -> std::io::Result<RelayHello> {
    decode_io(body).map_err(|error| match decode::<HelloVersion>(body) {
        Ok(version) if version.relay_proto_ver != RELAY_PROTO_VER => std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            SkewedHello {
                relay_proto_ver: version.relay_proto_ver,
            },
        ),
        _ => error,
    })
}

/// Daemon side, phase 1 — read the [`RelayHello`] frame. Called by the accept loop AFTER it has
/// already consumed the 8-byte [`RELAY_MAGIC`] preamble it peeked to route the connection here.
pub async fn read_hello<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<RelayHello> {
    let body = read_frame(reader).await?;
    decode_hello(&body)
}

/// Daemon side, phase 1 — consume the [`RELAY_MAGIC`] preamble the accept loop peeked and read
/// the [`RelayHello`], both within [`HELLO_TIMEOUT`].
pub async fn accept_hello<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<RelayHello> {
    let hello = async {
        let mut magic = [0u8; RELAY_MAGIC.len()];
        reader.read_exact(&mut magic).await?;
        if magic != RELAY_MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "relay preamble mismatch",
            ));
        }
        read_hello(reader).await
    };
    tokio::time::timeout(HELLO_TIMEOUT, hello).await.map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("no relay hello within {}s", HELLO_TIMEOUT.as_secs()),
        )
    })?
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

    /// A relay of protocol 1 named an `agent` (an `AgentId`, a string on the wire) and no
    /// principal; its hello must still be read as far as its version, so the daemon can decline it
    /// as skewed.
    #[test]
    fn an_older_relays_hello_is_recognised_as_skewed() {
        #[derive(Serialize)]
        struct HelloV1 {
            relay_proto_ver: u32,
            root: std::path::PathBuf,
            view: String,
            agent: String,
        }
        let body = encode(&HelloV1 {
            relay_proto_ver: 1,
            root: "/repo".into(),
            view: "working".to_string(),
            agent: "a".to_string(),
        })
        .expect("encode");
        let error = decode_hello(&body).expect_err("a v1 hello does not decode");
        let skewed = error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<SkewedHello>())
            .expect("recognised as skewed");
        assert_eq!(skewed.relay_proto_ver, 1);
        assert!(decode_hello(b"\x01garbage").is_err());
    }

    #[test]
    fn relay_hello_round_trips_through_msgpack() {
        let hello = RelayHello {
            relay_proto_ver: RELAY_PROTO_VER,
            root: std::path::PathBuf::from("/repo/root"),
            view: "main".to_string(),
            principal: Principal::parse("claude-code").expect("principal"),
            host: None,
            notices: Vec::new(),
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
            detail: None,
        };
        let bytes = encode(&welcome).expect("encode");
        let back: RelayWelcome = decode(&bytes).expect("decode");
        assert_eq!(welcome, back);
    }

    /// The welcome as a build without `detail` declares it. Both directions must decode: that
    /// build's daemon answers this client, and that build's client reads this daemon's refusal.
    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct WelcomeWithoutDetail {
        relay_proto_ver: u32,
        daemon_version: String,
        accepted: bool,
        code: Option<String>,
    }

    #[test]
    fn a_welcome_decodes_across_builds_with_and_without_the_detail() {
        let old = WelcomeWithoutDetail {
            relay_proto_ver: RELAY_PROTO_VER,
            daemon_version: "0.34.0".to_string(),
            accepted: false,
            code: Some(decline::HOST_BUILD_FAILED.to_string()),
        };
        let read: RelayWelcome = decode(&encode(&old).expect("encode")).expect("a welcome without detail");
        assert_eq!(read.code.as_deref(), Some(decline::HOST_BUILD_FAILED));
        assert_eq!(read.detail, None);

        let new = RelayWelcome {
            relay_proto_ver: RELAY_PROTO_VER,
            daemon_version: "0.34.1".to_string(),
            accepted: false,
            code: Some(decline::WORKSPACE_LOCKED.to_string()),
            detail: Some("pid 7".to_string()),
        };
        let read: WelcomeWithoutDetail = decode(&encode(&new).expect("encode")).expect("a welcome with detail");
        assert_eq!(read.code.as_deref(), Some(decline::WORKSPACE_LOCKED));
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

    /// A frame's body is read as it arrives; a short one is an error naming what was missing.
    #[tokio::test]
    async fn a_frame_shorter_than_its_length_is_refused() {
        use tokio::io::AsyncWriteExt;

        let (mut writer, mut reader) = tokio::io::duplex(4096);
        writer.write_all(&1000u32.to_be_bytes()).await.expect("len");
        writer.write_all(&[7u8; 10]).await.expect("body");
        drop(writer);
        let error = read_frame(&mut reader).await.expect_err("short frame");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(error.to_string().contains("10 of 1000"), "{error}");
    }

    /// A daemon of relay protocol 1 drops a hello it cannot decode; the relay says so rather than
    /// reporting a bare end of stream.
    #[tokio::test]
    async fn a_daemon_that_closes_without_a_welcome_is_named_as_likely_older() {
        use tokio::io::AsyncReadExt;

        let (mut client, mut server) = tokio::io::duplex(4096);
        let hello = RelayHello {
            relay_proto_ver: RELAY_PROTO_VER,
            root: std::path::PathBuf::from("/repo/root"),
            view: "main".to_string(),
            principal: Principal::parse("claude-code").expect("principal"),
            host: None,
            notices: Vec::new(),
        };
        let server_task = tokio::spawn(async move {
            let mut magic = [0u8; RELAY_MAGIC.len()];
            server.read_exact(&mut magic).await.expect("read magic");
            let _ = read_frame(&mut server).await.expect("read the hello frame");
            drop(server);
        });
        let error = client_handshake(&mut client, &hello)
            .await
            .expect_err("no welcome came");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(error.to_string().contains("older basemind"), "{error}");
        server_task.await.expect("server task");
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
            principal: Principal::parse("claude-code").expect("principal"),
            host: None,
            notices: Vec::new(),
        };
        let welcome = RelayWelcome {
            relay_proto_ver: RELAY_PROTO_VER,
            daemon_version: "9.9.9".to_string(),
            accepted: true,
            code: None,
            detail: None,
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
