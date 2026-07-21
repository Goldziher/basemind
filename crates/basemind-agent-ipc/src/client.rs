//! The front-end half: [`UdsAgentClient`], a cross-process [`AgentClient`].
//!
//! It connects to a daemon-hosted engine over a Unix socket and satisfies the same trait as the
//! in-process client, so a UI written against `impl AgentClient` drives it with no change. The read
//! half is owned behind `&mut self` for [`next_event`](AgentClient::next_event); the write half sits
//! behind a `Mutex` so [`send_command`](AgentClient::send_command) can take `&self`, matching the
//! trait's shape.

use std::io;
use std::path::Path;
use std::sync::Arc;

use basemind_agent::{AgentClient, AgentCommand, AgentEvent};
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use crate::error::IpcError;
use crate::frame::{codec, decode, encode};

/// A cross-process client for a daemon-hosted agent engine. Speaks length-delimited msgpack over a
/// Unix socket; drop-in for [`InProcAgentClient`](basemind_agent::InProcAgentClient).
pub struct UdsAgentClient {
    reader: FramedRead<OwnedReadHalf, LengthDelimitedCodec>,
    writer: Arc<Mutex<FramedWrite<OwnedWriteHalf, LengthDelimitedCodec>>>,
}

impl UdsAgentClient {
    /// Connect to a daemon listening on `socket_path`.
    pub async fn connect(socket_path: impl AsRef<Path>) -> Result<Self, IpcError> {
        let stream = UnixStream::connect(socket_path.as_ref()).await?;
        Ok(Self::from_stream(stream))
    }

    /// Wrap an already-connected stream. Used by tests that pair the client to a `serve_connection`
    /// over a bound socket without going through a daemon.
    pub fn from_stream(stream: UnixStream) -> Self {
        let (read_half, write_half) = stream.into_split();
        Self {
            reader: FramedRead::new(read_half, codec()),
            writer: Arc::new(Mutex::new(FramedWrite::new(write_half, codec()))),
        }
    }
}

impl AgentClient for UdsAgentClient {
    async fn next_event(&mut self) -> Option<AgentEvent> {
        match self.reader.next().await {
            // A frame we cannot decode is a protocol skew; `.ok()` reports the stream as ended
            // rather than spinning on garbage. A framing error or a clean EOF end it the same way. ~keep
            Some(Ok(frame)) => decode::<AgentEvent>(&frame).ok(),
            Some(Err(_)) | None => None,
        }
    }

    async fn send_command(&self, command: AgentCommand) -> io::Result<()> {
        let frame = encode(&command).map_err(io::Error::other)?;
        let mut writer = self.writer.lock().await;
        writer.send(frame).await
    }
}
