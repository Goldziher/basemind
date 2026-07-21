//! Length-delimited msgpack framing shared by the client and server halves.
//!
//! The wire shape is `[u32 big-endian length][msgpack body]` — the exact framing basemind's comms
//! transport uses (`src/comms/`), so the two transports stay convention-compatible. The length prefix
//! and cap are handled by [`LengthDelimitedCodec`]; the body is msgpack via `rmp_serde` in its named
//! (map-keyed) form, which keeps frames self-describing and additive-field tolerant.

/// Defensive cap on a single wire frame. Matches basemind's comms `MAX_FRAME_BYTES` so both
/// transports agree on the largest frame either will accept.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

// The codec helpers below back the Unix socket halves only; gated so a non-Unix build (which has no
// client/server module) carries no unused-code. ~keep
#[cfg(unix)]
use serde::Serialize;
#[cfg(unix)]
use serde::de::DeserializeOwned;
#[cfg(unix)]
use tokio_util::bytes::Bytes;
#[cfg(unix)]
use tokio_util::codec::LengthDelimitedCodec;

#[cfg(unix)]
use crate::error::IpcError;

/// Build the length-delimited codec (u32 big-endian length prefix) with the frame cap applied.
#[cfg(unix)]
pub(crate) fn codec() -> LengthDelimitedCodec {
    let mut codec = LengthDelimitedCodec::new();
    codec.set_max_frame_length(MAX_FRAME_BYTES);
    codec
}

/// Encode a value into a msgpack frame body; the codec prepends the length prefix on send.
#[cfg(unix)]
pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Bytes, IpcError> {
    Ok(Bytes::from(rmp_serde::to_vec_named(value)?))
}

/// Decode a msgpack frame body into a value.
#[cfg(unix)]
pub(crate) fn decode<T: DeserializeOwned>(frame: &[u8]) -> Result<T, IpcError> {
    Ok(rmp_serde::from_slice(frame)?)
}
