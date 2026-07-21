//! The transport error type.

use std::io;

use thiserror::Error;

/// A failure on the cross-process agent transport: a socket IO error, or a msgpack encode/decode
/// error on a frame body.
#[derive(Debug, Error)]
pub enum IpcError {
    /// A socket read/write or connect failure.
    #[error("agent ipc transport io: {0}")]
    Io(#[from] io::Error),
    /// Encoding a command/event to msgpack failed.
    #[error("encoding an agent frame: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    /// Decoding a msgpack frame body failed (typically a protocol skew between peers).
    #[error("decoding an agent frame: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
}
